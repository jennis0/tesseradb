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
use tessera_store::manifest::{
    AttrExtent, DeclaredScalar, FileDigest, ManifestVocabulary, RecordExtent, SegmentDescriptor,
};
use tessera_store::render_presence::RENDER_PRESENCE_DIR;
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
    /// Live segments in any one view at or above which a fold is worth running *inside the
    /// window*. Below it the window passes and nothing happens.
    ///
    /// **The low threshold of two.** It answers "is there enough here to be worth a quiet-hours
    /// fold"; [`Self::max_segments`] answers "is this bad enough that it cannot wait".
    pub window_min_segments: usize,
    /// Live segments in any one view at or above which a fold is dispatched **at any hour**;
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
    /// Inside the daily window, with a view over `window_min_segments`.
    Window,
    /// A view reached `max_segments`, at whatever hour — segment growth past the point where
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
    /// The largest live segment count across the partition's views.
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
/// `last_fold_unix` are seconds; `live_segments` is the largest live segment count across views,
/// since compaction §9's gauge is per (partition, view) and any view over the threshold is worth
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
/// dispatches nothing anyway: a fold leaves one segment per partition-view and an overlay with the
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

/// One live segment of one view, as the plan names it.
///
/// `dir` is **prefix-relative**, so the plan is a list of names rather than of resolved paths — the
/// same form the manifest's `files` map takes, and the form [`execute`] joins onto whichever prefix
/// directory it is reading.
pub(crate) struct PlannedSegment {
    pub(crate) seg_id: String,
    pub(crate) dir: String,
}

/// One view's half of a fold plan.
pub(crate) struct FoldViewPlan {
    pub(crate) view: String,
    /// The incarnation of `view` this plan folds (decision 0115) — the bundle's own, which
    /// `Bundle::with_views` has already held to the live roster. Stamped into the new base
    /// segment so that the fold's output is the successor's and not a predecessor's.
    pub(crate) incarnation: tessera_types::view::ViewIncarnation,
    /// Every live segment of this view at the snapshot — the base plus every extent. Pass 1 merges
    /// them all; order does not matter to it, since the merge is driven by a heap over each
    /// cursor's `(morton, tessera_id)` key.
    pub(crate) segments: Vec<PlannedSegment>,
    /// The bound for this view's new `permutation.bin`: **one past the highest entity the
    /// snapshot's row space covers**, and emphatically not the live `entity_id_high_water`.
    ///
    /// `RowSpace::with_extent` refuses an extent that begins below the base permutation's bound, so
    /// a bound taken from the allocator's high-water would refuse every carried-forward flush
    /// segment whose entities were *buffered* at the snapshot — and it would refuse them at
    /// `open_written_prefix`, which runs after `CURRENT` has already flipped. Taken from the row
    /// space, the floor is exactly what every post-snapshot publication had to clear to become live
    /// in the first place.
    pub(crate) permutation_bound: u64,
    /// The rows this view holds at the snapshot, base and extents together. This is an upper
    /// bound on the new base's, since every row pass 1 drops is a deletion, and it is what
    /// [`memory_estimate`] charges pass 2b's image against. Nothing else reads it. The exact
    /// figure is not available until pass 1 has run.
    pub(crate) rows: u64,
}

/// One fold's immutable plan: the files it consumes, and `D₀`.
///
/// **Pure, and it holds nothing open.** Every input is an immutable file named by path, so the plan
/// survives arbitrary churn on the executor while the fold runs — what it does *not* survive is a
/// publication that consumed one of those files, which is what the rebase check at publication
/// (compaction §4 step 1) is for.
pub(crate) struct FoldPlan {
    pub(crate) partition: String,
    pub(crate) views: Vec<FoldViewPlan>,
    /// Every live delta tier, prefix-relative, in the live manifest's order — all consumed.
    pub(crate) tiers: Vec<String>,
    /// Every live external-id run, prefix-relative, oldest first — all consumed by pass 3.
    pub(crate) runs: Vec<String>,
    /// Every live locator extent's path — consumed with the runs they index.
    pub(crate) locator_extents: Vec<String>,
    /// Every attribute extent the partition's side-manifest named at the snapshot — **all**
    /// consumed and folded into the new base columns (filter-index §6.2). Carrying an untouched
    /// one forward is declined there: it trades the objective — zero extents after a fold — for IO
    /// the fold can afford, and makes the folded state a function of deletion history rather than
    /// of the schema.
    pub(crate) attr_extents: Vec<AttrExtent>,
    /// Every record-blob extent the side-manifest named at the snapshot — **all** consumed and
    /// folded into the new base blob, under [`FoldPlan::attr_extents`]'s all-or-nothing argument:
    /// the folded state is a function of the schema, never of deletion history.
    pub(crate) record_extents: Vec<RecordExtent>,
    /// The entity→term transpose's extents at the snapshot — folded into the new base by pass 4c,
    /// exactly as the record extents are folded into the new blob.
    pub(crate) entity_terms_extents: Vec<tessera_store::manifest::EntityTermsExtent>,
    /// Every text extent the side-manifest named at the snapshot — **all** consumed and merged into
    /// the new base index, under the same all-or-nothing argument.
    pub(crate) text_extents: Vec<tessera_store::manifest::TextExtent>,
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
    /// No partition, or a partition with no view holding a segment. Nothing to fold.
    NothingToFold,
    /// **The estimated peak memory is above what the host has available** (compaction §3). Both
    /// figures in bytes.
    InsufficientMemory { need: u64, available: u64 },
    /// **The estimated output is above the free space on the device** (compaction §8). Both figures
    /// in bytes.
    InsufficientDisc { need: u64, free: u64 },
}

impl NoFold {
    /// Every gate, in the order [`NoFold::index`] numbers them.
    ///
    /// **`&'static str` from a closed list, and that is what makes a leak impossible here.** These
    /// names reach `/control/status`, which is the operator plane and not the viewer plane, but
    /// the rule is the same one the WAL pin's names follow (`ArtifactStore::wal_pin`): a
    /// `format!("{:?}", reason)` would publish whatever a future variant carries, and a variant
    /// naming a layer, a view or a partition would then put corpus-derived text on a status
    /// response without anything in the type system objecting. A fixed array of literals cannot
    /// carry a value at all.
    ///
    /// The names are the variant names in snake case, so a status field maps back to the arm that
    /// produced it.
    pub(crate) const GATES: [&'static str; 6] = [
        "wal_poisoned",
        "overlay_diverged",
        "stepped_down",
        "nothing_to_fold",
        "insufficient_memory",
        "insufficient_disc",
    ];

    /// This gate's position in [`NoFold::GATES`], and in the per-gate counters keyed by it.
    ///
    /// Exhaustive, so a new variant is a compile error here rather than a refusal counted under
    /// somebody else's name. `nofold_gates_are_named_and_numbered_once` is what holds the array
    /// and this match in step.
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

    /// The gauge form: a stable name for the condition, its counter's index, and the two figures
    /// where it carries them.
    ///
    /// **The refusal has to leave the process, and this is the only route out.** A refused fold
    /// advances no counter the `compaction` block publishes: `folds` does not move because nothing
    /// was folded, and `fold_failures` does not because a refusal is not a failure.
    ///
    /// The pair is `None` for the four conditions that carry no figures; for the two that do,
    /// `need` is the estimate and `had` is what the host answered — `MemAvailable` for the memory
    /// gate, `statvfs`'s `f_bavail` for the disc one.
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
    /// Roaring containers across every artifact membership this node holds — the artifact pass's
    /// price, and the one term of it a planner cannot derive. See
    /// [`ArtifactStore::membership_containers`](tessera_lifecycle::membership::ArtifactStore::membership_containers)
    /// for why it is counted from live state rather than modelled from the manifest, and
    /// [`ARTIFACT_BYTES_PER_CONTAINER`] for what it is charged at.
    pub(crate) membership_containers: u64,
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

/// Workers the fold's term-image derivation runs across.
///
/// **One**, because [`execute`] runs on one dedicated thread. A fold's input is the corpus, and
/// occupying request-serving workers for the length of one is the maintenance schedule reaching
/// the request path (decision 0043). Sequential also bounds the pass's memory to the one image and
/// the one scratch [`memory_estimate`] charges. The build passes `rayon::current_num_threads()`
/// instead. The bytes are identical either way: the derivation reads and appends a window at a
/// time in term order, so the width is a choice about the host and not about the file.
const TERM_IMAGE_THREADS: usize = 1;

/// The publication number the fold's term-image files are named after
/// (`tessera_store::derived::term_image_file`).
///
/// **Zero, and the fold cannot do better.** A derived file is named after the publication that
/// introduces it, and a fold's side-manifest number is allocated on the executor at publication,
/// hours after this pass writes the file. It has to be: a number taken at dispatch would sit below
/// every flush that published during the flight, and the fold's `SEGMENTS-<n>.json` would then lose
/// to theirs at the next open. What the number is for is uniqueness within a prefix, and that holds
/// here without it. A fold writes into a prefix it has just created, images are written once per
/// prefix by whichever publication creates it, and no flush, merge or coalesce writes this kind at
/// all (ruling 5, `docs/evidence/memos/2026-09-17-term-images-handover.md`). The build names its
/// own files from the same zero, being publication zero.
const TERM_IMAGE_MANIFEST_N: u64 = 0;

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
/// | 90 B × membership containers | the artifact pass's row forms, held while it rebuilds them |
/// | threads × (8 KiB × containers + scratch) | pass 2b's widest image and its scratch, modelled |
///
/// The permutation term is the **maximum** across views rather than their sum: pass 1 folds one
/// view at a time and drops each view's writer before the next, so the peak is one of them. The
/// image term takes its rows the same way, and for the same reason: pass 2b derives one view at a
/// time and drops each row space before the next.
///
/// **The image term is [`term_image_estimate`], modelled, and a ceiling rather than an
/// expectation.** [`TERM_IMAGE_THREADS`] of them, each with a [`PROJECT_SCRATCH_BYTES`] scratch.
/// A Roaring container covers 65 536 rows and costs at most 8 KiB, at which point it is a bitset
/// covering every row in its range, so a term held by every row of the view is the widest image
/// expressible and no posting can produce a larger one. The realistic figure is far below it:
/// a term over a third of the corpus in run-friendly order is kilobytes. ~437 MB at 3.5×10⁹ rows,
/// against ~82 MiB of scratch (`docs/evidence/memos/2026-09-17-term-images-handover.md` §3.4).
///
/// *(§3's first draft called the two mapped arrays free — page cache rather than RSS. r1 corrected
/// it: a dirty shared file mapping is resident and cgroup-charged until writeback. They are charged
/// here on r1's reading, which is also the conservative one.)*
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
        .saturating_add(term_image_estimate(dict_len, base_rows));
    terms.saturating_mul(FOLD_MEMORY_SAFETY_FACTOR)
}

/// What [`ProjectScratch`](tessera_store::permutation::ProjectScratch) holds at its widest, in
/// bytes.
///
/// **Arithmetic from two constants, and a bound rather than a typical figure**: the projection
/// emits and clears its buckets every 64 MiB of row ids whatever the mask, so what it holds is one
/// window plus a partly filled chunk per bucket, at most 82 MiB at the 1,025 buckets of the `u32`
/// entity ceiling. `permutation.rs` states it beside the window it follows from, and the anonymous
/// peaks it produces are measured there.
const PROJECT_SCRATCH_BYTES: u64 = 82 * 1024 * 1024;

/// The widest a Roaring container can be once built, in bytes: a bitset over its 65 536 values.
const IMAGE_BYTES_PER_CONTAINER: u64 = 8 * 1024;

/// Rows one Roaring container covers.
const ROWS_PER_CONTAINER: u64 = 1 << 16;

/// [`memory_estimate`]'s pass 2b term: one worker's largest possible image and its scratch, per
/// worker.
///
/// **Zero where the pass does not run**, which is a view with no row and a dictionary with no term:
/// pass 2b skips both, so charging a scratch for them would refuse folds for work nothing does.
fn term_image_estimate(dict_len: u64, base_rows: u64) -> u64 {
    if dict_len == 0 || base_rows == 0 {
        return 0;
    }
    let image = base_rows
        .div_ceil(ROWS_PER_CONTAINER)
        .saturating_mul(IMAGE_BYTES_PER_CONTAINER);
    (TERM_IMAGE_THREADS as u64).saturating_mul(image.saturating_add(PROJECT_SCRATCH_BYTES))
}

/// What one Roaring container costs resident, in bytes — the artifact pass's whole price model.
///
/// **Measured, not assumed**: 78.5–94.0 B per container across three decades of artifact count and
/// two membership shapes, flat, because the cost is per *container* rather than per artifact or per
/// member ([the residency probe](../../../probes/2026-08-16-membership-residency/README.md)). 90 is
/// the realistic arm's figure, and the pessimistic arm is *cheaper* per container — the scattered
/// case pays by holding more of them, which is exactly what counting containers rather than
/// artifacts captures.
///
/// The cross-check is the pass itself: 10⁷ artifacts of four runs each measured **+3.5 GB** for the
/// rebuilt row forms, against 90 B × 4×10⁷ containers = 3.6 GB
/// ([the pass probe](../../../probes/2026-08-16-fold-artifact-pass/README.md)). The two probes
/// arrive at the figure independently, which is why this is a constant and not a factor.
const ARTIFACT_BYTES_PER_CONTAINER: u64 = 90;

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
    let (partition, partition_data) = generation
        .bundle
        .partitions
        .iter()
        .next()
        .ok_or(NoFold::NothingToFold)?;
    let manifest = &partition_data.manifest;

    let mut views: Vec<FoldViewPlan> = Vec::new();
    // Sorted, so a plan is a function of the generation and not of a `HashMap`'s iteration order —
    // the new manifest's segment list follows this order, and a manifest whose bytes depend on
    // hashing is a bundle identity that depends on hashing.
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

    // The partition-wide locator span. Every post-snapshot locator extent begins above this, for
    // the same reason every post-snapshot segment does — `with_extent`'s floor — and publication
    // checks that rather than assuming it.
    let entity_bound = views
        .iter()
        .map(|view| view.permutation_bound)
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
            views
                .iter()
                .map(|view| view.permutation_bound)
                .max()
                .unwrap_or(0),
            entity_bound,
            u64::from(dict_len),
            resources.membership_containers,
            // The widest view's rows, on the permutation term's rule: pass 2b derives one view at
            // a time.
            views.iter().map(|view| view.rows).max().unwrap_or(0),
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
    /// **One writer schema per view**, keyed by view id: the bundle-wide render tail plus that
    /// view's group-scoped render lanes (`views.md` §5, `write::view_scalar_schema_of`). A single
    /// bundle-wide schema was the defect — a fold rewriting a group's view dropped the family's
    /// lane, and its values came back as the type's zero.
    pub(crate) scalar_schema: BTreeMap<String, Vec<(String, ScalarType)>>,
    /// Per view, the columns of its schema an input segment may lawfully lack
    /// (`write::lawful_absences`): the view's group-scoped lanes and the runtime columns below.
    /// A column missing for any other reason is a torn segment, and pass 1 refuses it rather
    /// than blanking the column and reclaiming the input.
    pub(crate) absent_ok: BTreeMap<String, Vec<String>>,
    /// The entity-scoped columns declared at a running service and not yet folded, by name, as
    /// the live list stood at the plan (`ingest.md` §6.3). None has a base: pass 4a folds each
    /// from its extents alone and writes the base every later reader opens, and the publication
    /// moves the column into `MANIFEST.json` and off the runtime list.
    pub(crate) runtime_attributes: Vec<String>,
    /// The group-scoped families declared at a running service and not yet folded, by name.
    /// Each view's column was based by the flush that first wrote it, so the pass reads them as
    /// it reads a build family's; the list is what the publication moves into `MANIFEST.json`.
    pub(crate) runtime_scoped_attributes: Vec<String>,
    /// The new base segment's id, one per view — never reused, so a discarded fold's orphans can
    /// never be mistaken for a later one's output (contracts §2.1).
    pub(crate) seg_id: String,
    /// The live base postings and the live tiers — pass 2's inputs.
    pub(crate) base_postings: Arc<PostingsReader>,
    pub(crate) tiers: Vec<Arc<DeltaTier>>,
    /// The bundle's declared scalars and its vocabularies — the attribute pass's own inputs, and
    /// the only thing that decides which columns it owes an artefact. Taken from the manifest
    /// rather than from the directory, for `FilterColumns::open`'s reason: a declared column whose
    /// files are missing is an error, and a directory scan finds what is there where a declaration
    /// says what must be.
    pub(crate) declared_scalars: Vec<DeclaredScalar>,
    /// Every group's **group-scoped** column families (`views.md` §5), flattened. The attribute
    /// pass owes each of them one folded column *per view of its group*, exactly as it owes an
    /// entity-scoped column one bundle-wide — and without them a fold writes a prefix in which the
    /// families' directories simply are not there, which is a bundle that does not open.
    pub(crate) scoped_scalars: Vec<tessera_store::manifest::ScopedScalar>,
    /// Which incarnation each view of the roster is, taken from the same manifest
    /// `scoped_scalars` came from (decision 0115). A scoped column's directory carries it above
    /// the build's, so the fold that rewrites those columns has to place them where the opener
    /// will look — the one derivation being `tessera_store::scoped_column_rel`.
    pub(crate) view_incarnations:
        std::collections::HashMap<String, tessera_types::view::ViewIncarnation>,
    pub(crate) vocabularies: Vec<ManifestVocabulary>,
}

/// The path one view's column of a scoped family folds into, or `None` where this manifest cannot
/// say which incarnation the view is (decision 0115).
///
/// **`None` skips the column rather than guessing one.** A family naming a view the roster cannot
/// place is a manifest whose two halves disagree; `FilterColumns::open` skips it for the same
/// reason, so the fold writing nothing there produces exactly the state the opener already
/// tolerates instead of a folded column at a path nothing reads.
fn scoped_job_rel(plan: &FoldPlan, ctx: &FoldContext, family: &str, view: &str) -> Option<String> {
    let incarnation = ctx.view_incarnations.get(view)?;
    Some(tessera_store::scoped_column_rel(
        &plan.partition,
        family,
        view,
        *incarnation,
    ))
}

/// One column the attribute pass folds: where its files live, and what its declaration says about
/// them. **One shape for both scopes** — an entity-scoped column at `attrs/<column>/` and one view
/// of a group-scoped family at `attrs/<column>/<group>/<key>/` — because the fold of a column is
/// the same merge either way, and the only thing the scope decides is the directory and which
/// extents belong to it (`views.md` §5).
struct ColumnJob {
    /// Prefix-relative directory, the same under both prefixes.
    rel: String,
    name: String,
    /// The view this column belongs to, for a scoped family — `None` for an entity-scoped column.
    /// The extent filter's other half: a family's columns share one name.
    view: Option<String>,
    arrow_type: ScalarType,
    /// Does the folded column owe rebuilt keyed postings? A category's, and only a category's —
    /// `d.vocabulary.is_some()` bundle-wide and `filter::scoped_owes_postings` per family, which
    /// is where the two scopes' rules differ (`filter-index.md` §2.3, `views.md` §5).
    postings: bool,
}

/// Every column the attribute pass folds, entity-scoped then group-scoped, in manifest order.
///
/// **The predicate is the opener's**, `filter::owes_value_column` and
/// `filter::scoped_owes_postings`, so the set of columns this pass writes and the set
/// `FilterColumns::open` demands are one rule: a column folded and not opened is dead bytes, and
/// one opened and not folded is a prefix that refuses at the first read.
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
        // Text owes no value column, per view exactly as bundle-wide: its whole index is a token
        // dictionary and the postings over it, which `fold_text_columns` merges.
        //
        // **The condition is the value column and not the filter licence**, and it must be the
        // same one `write_scoped_extents` writes on: a family carrying neither `index` nor
        // `render` is stored and served at the drill-down (owner ruling), so its column has
        // layers, and a fold that skipped it would leave those layers behind — the extents it
        // folded away are gone from the manifest and the values with them.
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

/// The process's resident set in bytes as a tuple — total, anonymous, file-backed — over
/// [`tessera_types::process::resident_bytes`], which reads it and states what zeros mean.
///
/// **Three numbers rather than one, because §3's budget is a claim about which of them grows.**
/// Two of the budget's terms — `permutation.bin` and `ext-locator.u32` — are written *through a
/// mapping*, so they land in `RssFile` and are reclaimable under pressure once written back; the
/// spool buffers and the term encodes are `RssAnon` and are not. A fold whose total climbs because
/// its inputs became resident is behaving as designed. One whose *anonymous* half climbs with the
/// corpus has a term nobody budgeted, and only the split tells the two apart.
fn resident_set() -> (u64, u64, u64) {
    let r = tessera_types::process::resident_bytes();
    (r.total, r.anon, r.file)
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

/// A staircase under construction: the rows recorded so far and the instant the next row is
/// measured from.
///
/// The fold thread starts one for its passes; `publish_fold` resumes it on the executor for the
/// publication's phases, so `/control/status` reports the fold from the thread's entry to the
/// superseded prefix's reclaim rather than the thread's half of it. Measured on rung 3
/// (`probes/2026-09-04-epoch-shard-fold-decomposition/`), the publication was half the fold's wall
/// and held the resident set's peak.
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
/// the publication logs about them.
///
/// The summary rides along rather than being recomputed from the file, because the wall clock and
/// the counts are the pass's own and nothing in the file records them.
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
    /// carried-forward files' digests and writes the result as `MANIFEST.json`.
    pub(crate) files: BTreeMap<String, FileDigest>,
    /// The new run 0's prefix-relative path — `None` when the deployment holds no external ids at
    /// all, in which case pass 3 wrote nothing and the new manifest lists no runs.
    pub(crate) external_id_run: Option<String>,
    /// One entry per view pass 2b wrote images for, in the plan's view order. The extents go into
    /// the new `SEGMENTS-<n>.json` unchanged: the images are the fold's own files under the fold's
    /// own prefix, so there is nothing for the publication to rebase.
    pub(crate) term_images: Vec<FoldedTermImages>,
    /// The largest new base segment's `columns.arrow + morton.u32 + cuts.u32` bytes — compaction
    /// §4 step 3's operand, computed here because these are the files that were just written.
    /// The same three files the server sums at startup for the merge-size relation
    /// (`tessera_server::validate_merge_size_relation`); they are mapped together, so a segment's
    /// size is all of them and the two computations of one quantity have to name one set.
    pub(crate) base_segment_bytes: u64,
    /// One [`PassCost`] per pass, in execution order — the fold thread's account of what it
    /// spent. Publication resumes the staircase with its own phases, logs the whole and reduces it
    /// to two gauges on `/control/status`.
    pub(crate) cost: Vec<PassCost>,
    /// When the fold thread's last row ended, from which the publication's first row is measured.
    /// The thread sets it after any test hold, so a held fold does not report the hold as the
    /// hand-off.
    pub(crate) finished: std::time::Instant,
    /// Attribute bytes pass 4a read and wrote (`filter-index.md` §6.2). **Reported, never
    /// triggered on** — the staircase attributes time and residency to the pass but not its IO,
    /// and IO is the term the non-disruption argument rests on.
    pub(crate) attr_bytes_read: u64,
    pub(crate) attr_bytes_written: u64,
    /// [`FoldContext::runtime_attributes`] and [`FoldContext::runtime_scoped_attributes`], the
    /// columns this fold gave a base and the publication moves off the runtime list.
    pub(crate) runtime_attributes: Vec<String>,
    pub(crate) runtime_scoped_attributes: Vec<String>,
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
    let mut stairs = Staircase::start();
    stairs.record("entry");

    // ---- pass 1 — row space -------------------------------------------------------------------
    //
    // One new segment per (partition, view), and one `permutation.bin` beside it. Rows whose
    // entity is in `D₀` are dropped, which shifts the row id of every row after them — the whole
    // reason compaction §6 exists.
    let mut segments: Vec<SegmentDescriptor> = Vec::with_capacity(plan.views.len());
    let mut base_segment_bytes = 0u64;
    for view in &plan.views {
        // **This view's own writer schema** (`views.md` §5). Refused rather than defaulted: an
        // empty schema writes a segment with no scalar tail at all, which every reader takes for a
        // corpus that declares none — the silent shape this whole change exists to remove.
        let Some(view_schema) = ctx.scalar_schema.get(&view.view) else {
            return Err(FoldFailed(format!(
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
                // `D₀`, whole and unmodified. See the module doc.
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
            written.push((format!("{segment_rel}/{name}"), path));
        }
        base_segment_bytes = base_segment_bytes.max(view_bytes);
        // The render columns' presence bitmaps beside the new segment (decision 0064), named by
        // the pass that decided which columns still have an absence after the drops. Not counted
        // into `view_bytes`, which is the three mapped files step 3's headroom check is about.
        for column in &out.presence_columns {
            written.push((
                format!("{segment_rel}/{RENDER_PRESENCE_DIR}/{column}.roaring"),
                tessera_store::render_presence::render_presence_path(&segment_dir, column),
            ));
        }
        written.push((permutation_rel, permutation_path));
        written.push((row_entity_rel, row_entity_path));

        segments.push(SegmentDescriptor {
            view: view.view.clone(),
            incarnation: view.incarnation,
            seg_id: ctx.seg_id.clone(),
            row_count: out.row_count,
            entity_lo: 0,
            // Inclusive, and the permutation's span rather than the highest surviving entity: this
            // is what the base *addresses*, and a range short of it would leave a carried-forward
            // extent's floor test asking about entities the base already owns.
            entity_hi: view.permutation_bound.saturating_sub(1),
        });
    }

    stairs.record("1 row space");

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
    written.push((postings_rel, postings_path.clone()));
    written.push((pairs_rel, pairs_path));
    stairs.record("2 postings");

    // ---- pass 2b: the term images -------------------------------------------------------------
    //
    // One file per view, each term's new base posting projected into the view's new row space
    // (`tessera_store::term_images`). Here rather than at publication because the derivation is
    // minutes of work at corpus scale and the executor must stay free to reach a queued deny; and
    // after pass 2 rather than beside pass 1 because the postings it reads are the ones pass 2 has
    // just written, from which every folded deletion is already gone. A deleted entity is in no
    // posting, so it is in no image, and that is the whole of the deletion rule reaching this
    // artefact. There is no second removal route (write-path §5.4).
    //
    // **The new base only.** Rows a later flush appends are an extent, and an extent gets no
    // images: a session unions the images of the terms it holds and walks the rest, and the rows
    // it arrives at are the same either way.
    let mut term_images: Vec<FoldedTermImages> = Vec::new();
    {
        let postings = PostingsReader::open(&postings_path, true)
            .map_err(|e| failed("pass 2b (term images: the new postings)", &e))?;
        let dict_len = postings.term_count();
        // The counter that names the files, as a publication's own does
        // (`tessera_store::derived::DerivedIndex`). This one belongs to the fold thread: the
        // publication's counter is created hours later and numbers the structures the executor
        // writes. The two cannot collide, because the kinds are different and this prefix is one
        // no other publication has ever written a term image into.
        let mut index = tessera_store::derived::DerivedIndex::default();
        // **Over the descriptors pass 1 pushed.** Each carries the view, its incarnation and the
        // rows the new base holds: the three fields the opener matches an entry on, and the two
        // the stamp must agree with. Reading them from the plan instead would be a second
        // statement of what pass 1 wrote.
        for segment in &segments {
            // Neither has an image to hold: projection maps entities to rows, and a view with no
            // row projects every posting to the empty set. The build's pass skips both for the
            // same reason, and a view with no entry is one the opener leaves walking.
            if segment.row_count == 0 || dict_len == 0 {
                continue;
            }
            let permutation_path = ctx.to_prefix_dir.join(format!(
                "partitions/{}/{}/permutation.bin",
                plan.partition,
                tessera_store::view_rel(&segment.view)
            ));
            // Reloaded from the file pass 1 wrote rather than kept from that pass, so the images
            // are a function of the published permutation. The build's pass loads it for the same
            // reason.
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

            // The one adapter between the postings format and the derivation: `tessera-store` does
            // not depend on `tessera-authz`, so the shape is handed across. `term_images_pass::run`
            // in `tessera-build` holds the identical six lines, and `containment` here holds them
            // for its own derivation.
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

            // Pass 5 digests and syncs what `written` names, the file's directory entry included
            // (`tessera_store::fsync_written`), so this pass syncs nothing of its own. The build's
            // does, because its digest pass has no such list.
            written.push((file.rel.clone(), file.path));
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
    }
    stairs.record("2b term images");

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

    stairs.record("3 external ids");

    // ---- pass 4a — the attribute artefact ------------------------------------------------------
    //
    // filter-index §6.2. One streaming pass per declared filter column: its base and every
    // snapshot extent merged in entity order into one new base, `D₀`'s entities blanked — removed
    // from presence, their value bytes never written — and a category's postings rebuilt whole
    // from the folded column, which is what makes the accelerator self-retiring rather than a
    // second durable identity.
    //
    // **What only the fold can do here is retention.** A layered column already queries within ~5%
    // of a single build's (measured, §5.1) and the coalesce bounds the file count continuously
    // (§5.2), but a deleted entity's *filter* value survives every other pass: its row is gone, so
    // its render value is gone with it, while the value column is positional and **I9** forbids
    // renumbering the slot away. This is where those bytes leave the corpus.
    //
    // **Nothing here is a third retirement rule.** A suppression touches no attribute artefact at
    // all (Rule S), and what this executes is exactly `D₀`, the same set passes 1–3 took.
    // **The pass reports its IO, because §6.2 asks for reporting and not for a gauge.** The
    // staircase already attributes time and resident bytes to `4a attributes`; what it cannot show
    // is that the pass is a *streaming* cost — ~12 GB per `u32` column at 10⁹, read, written and
    // re-read for the digest — which is the term the non-disruption argument turns on. Bytes, not a
    // trigger: §5.2's coalesce bounds the extent axis continuously and the segment axis is gauged
    // already, so there is nothing here for a threshold to do.
    let mut attr_read = 0u64;
    let mut attr_written = 0u64;
    let file_len = |path: &std::path::Path| std::fs::metadata(path).map(|m| m.len()).unwrap_or(0);

    // **A text column's index, merged and blanked.** Its own pass, before the value columns,
    // because this family owes no value column at all — its whole index is a token dictionary and
    // postings over it, and the generic merge below would have nothing to merge.
    fold_text_columns(&plan, &ctx, &mut written, &mut attr_read, &mut attr_written)?;

    for job in value_column_jobs(&plan, &ctx) {
        let scalar = &job;
        let column_rel = job.rel.clone();
        let from_dir = ctx.from_prefix_dir.join(&column_rel);
        let to_dir = ctx.to_prefix_dir.join(&column_rel);
        std::fs::create_dir_all(&to_dir).map_err(|e| failed("pass 4a (attributes)", &e))?;

        // **Advised `MADV_SEQUENTIAL`, and these are the fold's own mappings** rather than the live
        // generation's: decision 0052's rule is that the hint belongs to the mappings the fold
        // owns, and the request path's `FilterColumns` must never be advised on the fold's behalf —
        // which its signature makes unexpressible. The merge below streams each layer exactly once
        // in entity order, so the readahead suits the access and the drop-behind is the point: these
        // pages are not wanted again, and the request path's are.
        // **A column declared at a running service has no base until this pass writes one**
        // (`ingest.md` §6.3): its layers are the extents alone, and a column no flush has carried
        // yet folds to an empty base, so the reopen finds the files every declared column owes.
        let unfolded = job.view.is_none() && ctx.runtime_attributes.contains(&job.name);
        let base = if unfolded {
            None
        } else {
            let base = tessera_filter::ValueColumn::open_dir(
                &from_dir,
                tessera_filter::Access::MappedSequential,
            )
            .map_err(|e| failed("pass 4a (attributes: the base column)", &e))?;
            attr_read += file_len(&from_dir.join(tessera_filter::VALUES_FILE))
                + file_len(&from_dir.join(tessera_filter::PRESENCE_FILE));
            Some(base)
        };
        let mut extents = Vec::new();
        for extent in plan
            .attr_extents
            .iter()
            .filter(|e| e.column == scalar.name && e.view == job.view)
        {
            extents.push(
                tessera_filter::open_extent(
                    &ctx.from_prefix_dir.join(&extent.values),
                    &ctx.from_prefix_dir.join(&extent.presence),
                    tessera_filter::Access::MappedSequential,
                )
                .map_err(|e| failed("pass 4a (attributes: an extent)", &e))?,
            );
            attr_read += file_len(&ctx.from_prefix_dir.join(&extent.values))
                + file_len(&ctx.from_prefix_dir.join(&extent.presence));
        }
        let layers: Vec<&tessera_filter::ValueColumn> = base.iter().chain(extents.iter()).collect();
        // A keyword layer's dictionary, opened beside its ordinals and in the same order, because
        // an ordinal names a position in *its own* layer's dictionary and nothing anywhere else.
        // Empty for every other family, which is what selects the generic fold below.
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
            for extent in plan
                .attr_extents
                .iter()
                .filter(|e| e.column == scalar.name && e.view == job.view)
            {
                let Some(dict_rel) = extent.dict.as_ref() else {
                    return Err(FoldFailed(format!(
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
        // The snapshot's entity space, which is what the folded column covers. A column dense to
        // this bound writes no presence bitmap at all — the reader's "the entity id is the array
        // index" — and one deletion below it is what takes that away.
        let bound = u32::try_from(plan.entity_bound).map_err(|_| {
            FoldFailed("pass 4a (attributes): the entity bound exceeds u32".to_string())
        })?;
        // **A keyword folds through its own pass, because its values are ordinals.** The generic
        // fold carries values through byte-preserved, which is exactly wrong for a column whose
        // dictionary is rebuilt from the survivors and whose ordinals must be renumbered against
        // it — a key whose only carrier was blanked leaves the corpus, which is the retention
        // argument reaching dictionary keys (records §7). The two passes are otherwise the same
        // merge under the same guards.
        let partial = if layers.is_empty() {
            // Nothing has carried the column: an empty base with an empty presence, which is
            // what a column no entity holds a value for is.
            write_empty_value_column(
                &values_path,
                &presence_path,
                column_kind_of(scalar.arrow_type, job.postings),
            )
            .map_err(|e| failed("pass 4a (attributes: an empty base)", &e))?;
            if scalar.arrow_type == tessera_spatial::tiler::ScalarType::Keyword {
                write_empty_dictionary(&dict_path)
                    .map_err(|e| failed("pass 4a (attributes: an empty dictionary)", &e))?;
                attr_written += file_len(&dict_path);
                written.push((dict_rel, dict_path.clone()));
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
            attr_written += file_len(&dict_path);
            written.push((dict_rel, dict_path.clone()));
            partial
        };
        attr_written += file_len(&values_path);
        written.push((values_rel, values_path.clone()));
        if partial {
            attr_written += file_len(&presence_path);
            written.push((presence_rel, presence_path.clone()));
        }

        if !job.postings {
            continue;
        }
        // **Rebuilt from the folded column**, read back rather than from the layers it was merged
        // from: that is what makes the postings a derivative of the artefact of record rather than
        // a second opinion about it, and it is the same emit the batch build calls.
        // **Mapped without the hint, unlike the merge's inputs above.** The banded emit scans this
        // column once per band (§6.2), and `MADV_SEQUENTIAL`'s drop-behind would turn every band
        // after the first into a re-read of bytes this pass has just written and still has in cache.
        // The advice is right for a single stream and wrong for a repeated one.
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
        attr_written += file_len(&postings_path);
        written.push((postings_rel, postings_path));
    }

    // ---- pass 4a, continued — the record blob --------------------------------------------------
    //
    // records §7: the blob is rewritten without the blanked entities' rows, base plus every
    // snapshot extent streamed in entity order into one new base — *remove, emit no bytes*, so a
    // deleted entity's prose is physically absent from the folded artefact. That retention
    // asymmetry is why the blob lives under `attrs/` and folds with everything else rather than
    // in a store the fold does not touch. **Rule F only**: the set blanked here is exactly `D₀`,
    // the same set every other pass took, and a suppression is not in it — a suppressed entity's
    // row streams through byte-preserved like any survivor's.
    //
    // The blob exists iff the schema declares a blob-resident column — the build's own predicate
    // (`write_record_blob`), so base presence is a function of the schema exactly as the column
    // artefacts' is. The mismatch arms are unreachable by construction and refuse loudly rather
    // than silently dropping extents' bytes.
    let blob_resident = ctx
        .declared_scalars
        .iter()
        .any(|d| crate::filter::blob_resident(d, &ctx.vocabularies));
    // The base blob exists where a column the build or an earlier fold declared is
    // blob-resident; a blob-resident column declared at a running service has extents alone
    // until this pass writes the base (`ingest.md` §6.3).
    let based_blob_resident = ctx.declared_scalars.iter().any(|d| {
        !ctx.runtime_attributes.contains(&d.name)
            && crate::filter::blob_resident(d, &ctx.vocabularies)
    });
    if !blob_resident && !plan.record_extents.is_empty() {
        return Err(FoldFailed(
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

        // The fold's own mappings, advised sequential like the value columns above (decision
        // 0052): each layer streams exactly once, block by block.
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
                attr_read += file_len(&from_dir.join(name));
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
            for rel in [&extent.blocks, &extent.hasrow, &extent.directory] {
                attr_read += file_len(&ctx.from_prefix_dir.join(rel));
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
            // A blob-resident column declared at a running service that no flush has carried:
            // an empty base, so the reopen finds the blob the schema says exists.
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
            attr_written += file_len(&path);
            written.push((rel, path));
        }
    }

    stairs.record("4a attributes");

    // ---- pass 4c — the entity→term transpose ---------------------------------------------------
    //
    // The same shape as the record blob's fold and the same retention: base plus every snapshot
    // extent, streamed in entity order into one new base, with `D₀`'s entities emitting nothing.
    // **Rule F only** — a suppression is not in `D₀`, and a suppressed entity's list streams
    // through unchanged, which is correct: a suppression hides an item and does not unlabel it.
    //
    // **No ordinal is remapped, and that is a property of the dictionary rather than a choice
    // here.** A stored ordinal is a position in the concatenation of `dict_extents` in listed
    // order; pass 4b carries that list forward verbatim by hard link, never renumbered and never
    // shrunk, and `coalesce_dict_extents` preserves positions for the same reason. So the numbers
    // this pass copies mean the same terms in the new prefix — unlike a keyword column's
    // ordinals, which are positions in a per-layer dictionary the fold rebuilds.
    //
    // Unconditional, unlike the blob's pass: every entity has a label set, so a base always
    // exists.
    {
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
            });
            for rel in [&extent.hasrow, &extent.offsets, &extent.terms] {
                attr_read += file_len(&ctx.from_prefix_dir.join(rel));
            }
        }
        for name in [
            tessera_store::ENTITY_TERMS_HASROW_FILE,
            tessera_store::ENTITY_TERMS_OFFSETS_FILE,
            tessera_store::ENTITY_TERMS_TERMS_FILE,
        ] {
            attr_read += file_len(&from_dir.join(name));
        }
        let layers = tessera_store::EntityTermsStack::open(Some(&from_dir), &extent_paths)
            .map_err(|e| failed("pass 4c (entity terms: the layers)", &e))?;
        let mut writer = tessera_store::EntityTermsWriter::create(&to_dir)
            .map_err(|e| failed("pass 4c (entity terms: the rewrite)", &e))?;
        // One ascending pass over the union of the layers' has-row sets, which is the order the
        // writer requires and the order every layer already holds.
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
            attr_written += file_len(&path);
            let rel = format!(
                "{terms_rel}/{}",
                path.file_name()
                    .and_then(|n| n.to_str())
                    .unwrap_or_default()
            );
            written.push((rel, path));
        }
    }

    // **Its own line, because it is its own pass.** The bytes above join `attr_read`/`attr_written`
    // — the fold's streamed-IO total covers every entity-space artefact it rewrites, and the
    // transpose is one — but the *time and resident bytes* are the staircase's business, and a
    // pass folded into its neighbour's row is a pass an operator reading the report cannot see.
    stairs.record("4c entity terms");

    // ---- pass 4b — the dictionary --------------------------------------------------------------
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
    // **This makes the fold's own output durable; publication does the same for what it links.** A
    // carried-forward file was written by a flush that did not sync it either — no producer here
    // syncs a data file — and a hard link copies no bytes, so `publish_fold` syncs the carry-forward
    // set before the flip for exactly the reason this pass syncs its own (compaction §8).
    let mut files = BTreeMap::new();
    for (rel, path) in &written {
        files.insert(
            rel.clone(),
            crate::flush::digest_of(path).map_err(FoldFailed)?,
        );
    }
    let paths: Vec<PathBuf> = written.iter().map(|(_, path)| path.clone()).collect();
    tessera_store::fsync_written(&paths).map_err(|e| failed("pass 5 (durability)", &e))?;
    // Pass 4b is not marked because it does nothing: the dictionary is carried forward by a link at
    // publication, and a zero-cost row in the staircase would read as an unmeasured one.
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
        attr_bytes_read: attr_read,
        attr_bytes_written: attr_written,
        runtime_attributes: ctx.runtime_attributes,
        runtime_scoped_attributes: ctx.runtime_scoped_attributes,
    })
}

/// Rebuild each indexed `text` column's index from the layers the snapshot named, minus `D₀`.
///
/// # Why this family needs a pass of its own
///
/// Every other indexed family stores one value per entity, so folding it is a merge of value
/// views and a presence subtraction, and its postings are then *re-derived* from the folded
/// column. A text column has no value column: its prose is a record-blob row and its index is a
/// token dictionary plus postings over it, so there is nothing for that merge to take. Deriving
/// the postings the same way — re-analysing every surviving row out of the folded blob — would
/// work and is what the family's other writers do, but it pays the analyser over the whole corpus
/// at every fold to reproduce a term set the layers already hold. **The postings are merged
/// instead**, which is a union per term and a subtraction, and reaches the same artefact because
/// the analyser is a pure function of the prose and every layer's terms came from it.
///
/// # Rule F, and the retention statement this pass carries
///
/// The blanked set is `D₀`, whole, the same set every other pass takes — a suppression is not in it
/// and touches no artefact here (Rule S). What `andnot` leaves is the whole of the deletion's
/// effect on this family, and it reaches further than the postings: **a term whose only carriers
/// were deleted is not written to the merged dictionary at all**, so the word itself leaves the
/// corpus. That is the same retention the keyword fold states for a dictionary key, and this is the
/// only place a deleted document's vocabulary can go.
///
/// Carrying the index forward untouched instead would leave a deleted entity's terms in the
/// postings while the fold retired its overlay entry — a `match` naming an entity nothing else in
/// the bundle admits exists, which is Rule F broken silently and fail-open.
///
/// # Streaming, and what it costs
///
/// One term at a time: the layers' dictionaries are merged by a k-way scan, each surviving term's
/// posting is encoded and appended to a spool, and the dictionary is written through
/// [`tessera_filter::SortedDictWriter`] as the merge decides each key. Resident cost is one term's
/// bitmap plus the spool's 8 B/term offsets buffer — the shape pass 2 takes, and the reason neither
/// pass materialises a `Vec<Vec<u32>>` the way the batch writers do.
///
/// ⊘ The spool's offsets buffer for these dictionaries is **not** in [`memory_estimate`], which
/// charges 8 B per *term-dictionary* ordinal only. A text column's vocabulary is a different and
/// smaller number — 991k terms over 2.4M abstracts, measured — and the estimate's ×2 safety factor
/// covers it at that scale; a deployment with many wide text columns would want it counted.
///
/// The merge decodes each key through `key_of`, which re-decodes its block prefix — `interval / 2`
/// discarded decodes per key, ~8 at the shipped interval. That is deliberate rather than
/// overlooked: a sequential cursor over the reader would be a fourth way to walk a dictionary, and
/// at 11–19 ns a decode the whole overhead is ~0.1 s per million terms per layer, against a pass
/// whose other terms are IO.
///
/// # What the folded layer does *not* carry
///
/// **No presence bitmap.** A flush extent stores one because an entity whose prose analysed to no
/// terms — an empty string, a line of punctuation — carries a value and appears in no posting, so
/// its layer would otherwise report it absent. The base build writes none, and the folded base is
/// the base: after this pass, "carries a value" is answered from the record blob, which holds the
/// prose itself. Nothing reads a text layer's presence today; when something does, the base owes
/// one and so does this pass.
fn fold_text_columns(
    plan: &FoldPlan,
    ctx: &FoldContext,
    written: &mut Vec<(String, PathBuf)>,
    attr_read: &mut u64,
    attr_written: &mut u64,
) -> Result<(), FoldFailed> {
    let file_len = |path: &Path| std::fs::metadata(path).map(|m| m.len()).unwrap_or(0);
    let failed = |what: &str, e: &dyn std::fmt::Display| FoldFailed(format!("{what}: {e}"));

    // The entity-scoped indexed text columns, then one job per view of each indexed **scoped**
    // text family (`views.md` §5): a family's per-view index is the same three artefacts in a
    // per-view directory, and the merge does not care which it is folding.
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
        let column_rel = job.rel.clone();
        let from_dir = ctx.from_prefix_dir.join(&column_rel);
        let to_dir = ctx.to_prefix_dir.join(&column_rel);
        std::fs::create_dir_all(&to_dir).map_err(|e| failed("pass 4a (text)", &e))?;

        // The base build's layer first, then one per published extent — the same files and the
        // same order `FilterColumns::open` composes, so the merge sees exactly what a request
        // would. Order decides nothing about the answer (the union is commutative and the layers
        // are disjoint in entity space by **I9**); it is kept because a manifest whose bytes depend
        // on an iteration order is a bundle identity that depends on one.
        //
        // The dictionaries are advised sequential, and they are the fold's own mappings rather
        // than the request path's (decision 0052): the merge streams each one exactly once, in
        // order. The postings are not — `ColumnPostings::open` takes no `Access`, and the merge's
        // access to them is *not* sequential anyway: it reads record `at[i]` of whichever layers
        // hold the least key, which walks each file in ordinal order but interleaved across
        // layers. `MADV_SEQUENTIAL`'s drop-behind would be wrong for that, not merely absent.
        // A text column declared at a running service has no base index until this pass writes
        // one (`ingest.md` §6.3): its layers are the extents alone.
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
            *attr_read += file_len(&from_dir.join(tessera_filter::DICT_FILE))
                + file_len(&from_dir.join("postings.arrow"));
        }
        for extent in plan
            .text_extents
            .iter()
            .filter(|e| e.column == scalar.name && e.view == job.view)
        {
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
            *attr_read += file_len(&ctx.from_prefix_dir.join(&extent.dict))
                + file_len(&ctx.from_prefix_dir.join(&extent.postings))
                + file_len(&ctx.from_prefix_dir.join(&extent.presence));
        }
        let dict_rel = format!("{column_rel}/{}", tessera_filter::DICT_FILE);
        let postings_rel = format!("{column_rel}/postings.arrow");
        let dict_path = ctx.to_prefix_dir.join(&dict_rel);
        let postings_path = ctx.to_prefix_dir.join(&postings_rel);
        let spool_path = to_dir.join("postings.spool");

        // **The layers' presence is not passed and there is nothing to pass it to.** This pass
        // writes a base, and a base carries no presence bitmap; the two-halves check the merge
        // makes on every input is the one guard a fold shares with the coalesce.
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
        // A fold's own spool files go on every exit path (compaction §8). `finish` removes it on
        // success; on failure it is this function's.
        if outcome.is_err() {
            let _ = std::fs::remove_file(&spool_path);
        }
        outcome?;

        *attr_written += file_len(&dict_path) + file_len(&postings_path);
        written.push((dict_rel, dict_path));
        written.push((postings_rel, postings_path));
    }
    Ok(())
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

/// An entity-space value column with no entity in it: the base a column declared at a running
/// service takes at its first fold when no flush has carried it (`ingest.md` §6.3). Written with
/// an empty presence bitmap, so the reader takes it as partial rather than as dense to the bound.
fn write_empty_value_column(
    values_path: &Path,
    presence_path: &Path,
    kind: tessera_filter::ColumnKind,
) -> std::io::Result<()> {
    tessera_filter::ValueColumnWriter::create(values_path, presence_path, kind)?
        .finish(Some(&Bitmap::new()))
}

/// A keyword column's dictionary with no key in it, beside [`write_empty_value_column`]'s
/// ordinals.
fn write_empty_dictionary(dict_path: &Path) -> std::io::Result<()> {
    let file = std::io::BufWriter::new(std::fs::File::create(dict_path)?);
    tessera_filter::SortedDictWriter::new(file)
        .and_then(|writer| writer.finish())
        .map(|_| ())
        .map_err(|e| std::io::Error::new(std::io::ErrorKind::InvalidData, e.to_string()))
}

/// The storage kind a value column of this declared type is written at: the build's
/// `column_kind`, over the manifest's type and whether the column is a category. A keyword's
/// values are `u32` ordinals; a `bool` stores as a `u8`, a `timestamp_us` as the `i64` it is.
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
        // Neither owes a value column: `utf8` is not declarable and `text` folds through its
        // own pass. Reaching here is a job the predicate above did not produce.
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
             FOLD_MEMORY_SAFETY_FACTOR on top, so ~17.9 GB is the figure"
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

    /// **Pass 2b is charged one image and one scratch, and only where it runs.** The image is a
    /// ceiling of a bitset container per 65 536 rows, so the term moves with the rows and not with
    /// the terms. The scratch is flat.
    ///
    /// Kills the mutation that charges the scratch to a fold with nothing to project, which would
    /// refuse folds on a small host for work the pass skips.
    #[test]
    fn the_estimate_charges_one_term_image_and_one_scratch() {
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
        // One container's rows, one term: one 8 KiB image and the scratch.
        assert_eq!(
            memory_estimate(0, 0, 1, 0, ROWS_PER_CONTAINER),
            (8 + IMAGE_BYTES_PER_CONTAINER + PROJECT_SCRATCH_BYTES) * FOLD_MEMORY_SAFETY_FACTOR
        );
        // One row past it takes a second container, and nothing else moves.
        assert_eq!(
            memory_estimate(0, 0, 1, 0, ROWS_PER_CONTAINER + 1),
            (8 + 2 * IMAGE_BYTES_PER_CONTAINER + PROJECT_SCRATCH_BYTES) * FOLD_MEMORY_SAFETY_FACTOR
        );
        // The memo's figure at rung 6: ~437 MB of image at 3.5×10⁹ rows.
        let image = term_image_estimate(1, 3_500_000_000) - PROJECT_SCRATCH_BYTES;
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
