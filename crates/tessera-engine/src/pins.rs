//! The pin manager: an `Arc` plus a drain list, with one ordering rule (I11, lifecycle §2.1–§2.3).
//!
//! # The rule, stated once
//!
//! **A pin fixes row-space geometry — `(prefix, segments_version)`, and the permutation, tile table
//! and columns those name — and never authorisation state.** Every guard in this file is one of the
//! two failures that rule names, and each is annotated with which:
//!
//! - **fail-open (R-open).** Answering a pinned request from a superseded generation's overlay,
//!   buffer or `overlay_version`. A suppression applies to a pinned request the moment it is
//!   accepted (lifecycle §2.3); a pin must not defer it. [`PinnedGeometry`] is where this is made
//!   structurally unavailable rather than remembered.
//! - **simply wrong (R-wrong).** Answering a pin against geometry other than the one it names.
//!   I11's own wording: "a row-space mask applied across a compaction boundary selects arbitrary
//!   rows — not stale-restrictive but simply wrong". It does not fail closed; it returns a `200`
//!   over unrelated items. [`check_publishable`] and [`PinManager::resolve_drained`]'s two refusals
//!   are the guards.
//!
//! Everything below **refers** to R-open and R-wrong rather than restating them. A comment that
//! restates a rule is a comment that can drift out of step with the code under it, which is not
//! hypothetical here: an earlier revision of `Engine::publish_geometry` carried a paragraph
//! claiming a guard the predicate underneath it could not deliver.
//!
//! # Why a drain list rather than an equality check
//!
//! A pin that is merely compared against the live generation expires every outstanding pin at the
//! instant geometry moves. A flush or a compaction supersedes a generation while requests are in
//! flight, and the pannable map's whole point is that a drill-down lands on the geometry the
//! viewport was drawn from. The drain list keeps a superseded geometry resolvable for a bounded
//! while ([`PinManager::retire`], [`PinManager::reclaim`]).
//!
//! # Four properties a reader should be able to check quickly
//!
//! 1. [`PinManager::resolve`] takes **no lock** unless a presented pin fails the live equality
//!    check. Every admitted request calls it, at the branch's 48-way admission concurrency, so a
//!    per-request global mutex here is a process-wide serialisation point on the viewport path —
//!    precisely the contention class the concurrency workstream's F4 work removed. This argument is
//!    made here and nowhere else in the file; the cold path is `#[cold]` and separated into its own
//!    function so the property is visible rather than argued, and [`PinManager::lock_drain`] is the
//!    **single** place this module locks, counting every acquisition so the property is testable.
//! 2. Reclaim is **remove → verify → drop**, never verify → remove (lifecycle §2.1: "verify-then-
//!    remove is the use-after-free the review caught").
//! 3. Lifecycle §2.2's TTL bites at [`PinManager::resolve`], not only at reclaim, so a lifecycle
//!    thread that has not run its reclaim pass cannot make an over-age pin resolvable again.
//! 4. **What bounds retention is the TTL, [`DEFAULT_DRAIN_DEPTH_MAX`], and a reclaim pass actually running
//!    — not §2.2's per-session cap.** See [`PinManager::pins_per_session_max`], and
//!    [`PinStats::oldest_retired_secs`] for the gauge that makes the third of those observable.

use std::sync::atomic::{AtomicU64, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex, MutexGuard, PoisonError};
use std::time::{Duration, Instant};

use tessera_store::Bundle;
use tessera_types::PinId;

use crate::session::EngineError;
use crate::Generation;

/// Drain depth above which an operator should alarm (lifecycle §2.2).
///
/// Depth 1 is ordinary: a geometry is published, the superseded prefix drains, and one entry sits
/// there until its TTL. Depth 2 means a *second* geometry was published before the first drained,
/// which is the shape the alarm exists to catch — at the probes' **measured** 47.02 GB bundle with
/// a 22.5 GB viewport-hot set, one slow client holding a pin across two publications contests
/// essentially the whole page cache of a 47 GB box, and holds up to a **modelled** ~94 GB of
/// invisible disk once prefix deletion lands (`df` disagreeing with `du`). The cost of losing that
/// cache is a warm-to-cold cliff: **measured** 4.0–4.5 ns per visible row warm, against a
/// **modelled** 50–100 µs per page miss. (The measured/modelled distinction is deliberate
/// throughout this project; do not flatten it.)
///
/// **An alarm above a saturated ceiling signals nothing** — see [`DEFAULT_DRAIN_DEPTH_MAX`]'s sizing
/// obligation, which is what keeps this gauge informative.
pub const DRAIN_DEPTH_ALARM: usize = 1;

/// The hard ceiling on drain depth. Retiring past it drops the **oldest** entries, whose pins then
/// `410` — fail-closed, and they were nearest their TTL anyway.
///
/// *An alarm is not a bound.* Without a ceiling, retention is publication rate × `pin_ttl_secs`,
/// and at the 300 s default with a per-flush `segments_version` advance that is tens of retained
/// bundles, each an `Arc<Bundle>` over mmapped segment files.
///
/// **What sizes it — and what does not.** This is **not** a memory budget, and must not be sold as
/// one. Four distinct prefixes at the probes' *measured* 47.02 GB bundle is a *modelled* ~188 GB of
/// mapped files: not holdable in RAM, not in page cache, and once [`PinManager::reclaim`] acquires
/// file deletion it is that much invisible disk. No ceiling large enough to be operationally useful
/// is small enough to be a byte bound — even depth 2 is ~94 GB. **Memory is bounded by
/// `pin_ttl_secs` and by a reclaim pass actually running** ([`PinStats::oldest_retired_secs`] is
/// the gauge for the second); this ceiling only keeps an *unbounded* list from existing.
///
/// So it is sized to the deepest legitimate publication pattern instead: depth 1 is one geometry
/// draining, depth 2 is the incident [`DRAIN_DEPTH_ALARM`] names, and **4** leaves one doubling of
/// headroom above that incident — enough that a legitimate burst degrades into an alarm rather than
/// into `410`s, and no more. (An earlier revision said 8, justified as "what a 47 GB box can hold".
/// Eight × 47.02 GB is ~376 GB; the arithmetic was simply false, and the figure it was reaching for
/// is not one any ceiling can deliver.)
///
/// **Sizing obligation on the stage that introduces a periodic publisher.** If publications land
/// closer together than `pin_ttl_secs / DRAIN_DEPTH_MAX` (75 s at the 300 s default), the list sits
/// at the ceiling permanently, the depth alarm saturates — stopping signalling exactly when depth
/// matters — and pins are dropped by the trim rather than by their TTL. Whichever of the two knobs
/// that stage sets, it must keep `pin_ttl_secs < DRAIN_DEPTH_MAX × publication_period`.
///
/// **A config key now, not a constant, and this constant is only its default.** The paragraph this
/// replaces argued the other way — "no key exists for this; if deployment experience wants it
/// tunable, that is a seam change" — and §1.4's cost model is what changed the answer rather than
/// deployment experience. The figure above was sized when a drain entry meant a whole distinct
/// bundle; under §1.2's incremental construction consecutive generations share their base geometry
/// by `Arc` and differ by a handful of small segments, so an entry costs roughly one flush segment.
/// That makes it the **cheaper** of the two knobs an admin has for visibility latency — raise this
/// rather than shorten `pin_ttl_secs` — and a knob compiled in is no knob.
///
/// *Modelled, not measured.* The figure to take before an admin leans on it is resident bytes per
/// drain entry under sustained flush.
///
/// The sizing obligation above is discharged by `tessera-server`'s loader, which refuses a
/// configuration violating `pin_ttl_secs < drain_depth_max × flush_max_age_secs` at startup.
pub const DEFAULT_DRAIN_DEPTH_MAX: usize = 4;

/// The most sessions one drain entry tracks against the per-session cap before it starts forgetting
/// the oldest.
///
/// Bounded because `holders` would otherwise be an unbounded, attacker-driven allocation on a path
/// entered only *after* a failed equality check: `Engine::authorise` mints a fresh `token_id` per
/// call and the fragment is cached, so rotating sessions is nearly free. Total bookkeeping is
/// bounded at `drain_depth_max × MAX_HOLDERS_PER_ENTRY × 8` bytes ≈ 32 KiB.
///
/// **The degradation past the ceiling runs in both directions**, and only one of them is
/// fail-closed:
///
/// - a forgotten session is charged for that pin again on its next presentation, so it may hit
///   [`EngineError::PinCapExceeded`] *sooner* than it should — fail-closed;
/// - and, the direction a one-sided reading misses, forgetting also makes that session's `held`
///   count *smaller*, so it can accumulate more simultaneously-resolvable geometries than
///   `pins_per_session_max` nominally allows. That over-run is bounded absolutely by
///   [`DEFAULT_DRAIN_DEPTH_MAX`] — a session cannot resolve more geometries than the list holds — and the
///   cap is an availability limit that never reaches an authorisation decision either way. It is
///   also bypassable outright by session rotation (see [`PinManager::pins_per_session_max`]), so
///   this ceiling is not the thing standing between a client and extra retained geometry.
const MAX_HOLDERS_PER_ENTRY: usize = 1024;

/// Sentinel for [`PinManager::oldest_retired_ms`] when the drain list is empty.
const NO_OLDEST: u64 = u64::MAX;

/// One superseded generation's geometry, kept resolvable until its TTL expires.
///
/// **Slimmed deliberately — NOT `Arc<Generation>`.** Holding a whole generation would also hold the
/// superseded *overlay* and *buffer*, which is R-open (see this module's doc), and would turn a
/// structural property of [`PinnedGeometry`] into a rule policed by whoever writes
/// [`PinManager::resolve`] next. The fields here are the fields `PinnedGeometry` has, plus the
/// three the drain list itself needs.
struct DrainEntry {
    prefix: String,
    segments_version: u64,
    /// Advisory only — see [`PinnedGeometry::watermark`], which this feeds.
    watermark: u64,
    bundle: Arc<Bundle>,
    /// When this geometry was superseded. The TTL is measured from here, on the monotonic clock: a
    /// wall-clock jump must not resurrect an expired pin or expire a live one. Retirement-based
    /// rather than mint-based because a mint has nothing to time — `PinId` carries only
    /// `(prefix, segments_version)` and this engine keeps no pin table — and because §2.2's own
    /// retention rule is retirement-based ("deletable only after marker + session-pin TTL").
    retired_at: Instant,
    /// The sessions (`Session::token_id`) currently holding this entry against lifecycle §2.2's
    /// per-session cap, oldest first, bounded by [`MAX_HOLDERS_PER_ENTRY`].
    ///
    /// **Identity, not a count.** A `usize` per session cannot be right in either direction: a
    /// session re-presenting the *same* pin would consume cap on every request until it locked
    /// itself out, and reclaim would have no way to give the cap back. Both fall out for free once
    /// the holder set lives on the entry — reclaim drops the entry and its holders with it, and a
    /// re-presentation is a membership test.
    ///
    /// A `Vec` with a linear scan, not a hash set: it is capped at 1024 `u64`s, scanned only on the
    /// cold path, and the ring eviction that bounds it needs insertion order, which a set does not
    /// keep. A revoked session's `token_id` also stays here until this entry is reclaimed — bounded
    /// by the TTL, and not worth a second lock plus a session-lifetime callback to shorten.
    holders: Vec<u64>,
}

impl DrainEntry {
    fn holds(&self, token_id: u64) -> bool {
        self.holders.contains(&token_id)
    }

    fn add_holder(&mut self, token_id: u64) {
        if self.holders.len() >= MAX_HOLDERS_PER_ENTRY {
            self.holders.remove(0);
        }
        self.holders.push(token_id);
    }
}

/// What one reclaim pass removed — the cache pruner's hook.
///
/// `segments_version` is exactly the [`crate::cache::RowProjectionKey`] component a
/// `prune_generation` takes, and it is deliberately delivered from *reclaim* rather than from the
/// generation swap: a pinned request still needs its generation's projection, and a swap-triggered
/// prune would delete the very key it is about to ask for.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Reclaimed {
    pub prefix: String,
    pub segments_version: u64,
    /// Whether this entry's `Arc<Bundle>` was uniquely held at the verify step — i.e. whether the
    /// drop that followed actually released this `Bundle`, or merely handed that job to the last
    /// in-flight request still holding a [`PinnedGeometry`] over it.
    ///
    /// Diagnostic, never a decision: the entry is removed from the drain list either way. Making
    /// removal conditional on this flag is the verify-then-remove ordering lifecycle §2.1 forbids.
    ///
    /// **It says less than lifecycle §2.1's phrasing implies.** §2.1's argument rests on per-file
    /// `Arc<Mmap>`s shared *across* generations, so that a file two generations reference survives
    /// either one's reclaim. Today `open_bundle` maps a prefix's files freshly on every call, so
    /// two `Bundle`s over the same prefix hold independent mappings and this flag is a statement
    /// about **this `Bundle` value** only — never about the page cache or about whether a file is
    /// still mapped somewhere else in the process.
    pub exclusively_held: bool,
}

/// Operator-facing pin gauges — the figures lifecycle §2.2 wants on `/control/status`, and what
/// that endpoint publishes as `pins`.
///
/// Every field is read from an atomic and **this type is produced without taking the drain lock** —
/// deliberately, so that `drain_locks` can be an honest measure of the type's own locking rather
/// than of the caller's polling.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PinStats {
    /// Entries currently on the drain list. Alarm above [`DRAIN_DEPTH_ALARM`]; entries past
    /// [`DEFAULT_DRAIN_DEPTH_MAX`] are dropped rather than retained.
    pub drain_depth: usize,
    /// **Every** acquisition of the drain mutex since process start, from anywhere in this module.
    ///
    /// This is the counter `resolve_takes_no_lock_when_no_pin_is_presented` asserts on, and it
    /// measures a property of [`PinManager`] rather than of one call path *because* there is
    /// exactly one place in this file that locks ([`PinManager::lock_drain`]). Any new lock added
    /// to this type must go through it, or that test silently stops covering property 4.
    pub drain_locks: u64,
    /// Age of the **oldest** drain entry, in whole seconds; `None` when the list is empty. Entries
    /// are pushed in retirement order and removed from the front, so this is exactly the age of the
    /// entry nearest its TTL.
    ///
    /// **The alarm this exists for, and why `drain_depth` cannot serve it.** Availability is
    /// bounded without a reclaimer — the TTL bites at [`PinManager::resolve`] (property 3) — but
    /// *memory* is released only by [`PinManager::reclaim`], and nothing in this crate calls that
    /// periodically. One publication, then a quiescent geometry, leaves a whole superseded bundle
    /// held indefinitely at **depth 1** — *at* [`DRAIN_DEPTH_ALARM`], which alarms only above it,
    /// so the state is invisible in every other gauge here.
    ///
    /// **`oldest_retired_secs >= pin_ttl_secs` means reclaim is not running.** The list is ordered,
    /// so that condition is exact rather than indicative: it holds iff at least one entry is past
    /// its TTL. Both halves of the comparison are in this struct so the check needs no other input.
    pub oldest_retired_secs: Option<u64>,
    /// Lifecycle §2.2's configured TTL, carried so [`Self::oldest_retired_secs`]'s alarm condition
    /// is self-contained.
    pub pin_ttl_secs: u64,
}

/// Why [`check_publishable`] refused a geometry publication.
///
/// An enum rather than a `&'static str` while there is exactly one reason: a caller that wants to
/// branch (an HTTP mapping, say) can, and adding a second reason is then a compile-time obligation
/// on every match rather than a new sentence nobody notices.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum GeometryRefusedReason {
    /// The offered `segments_version` does not strictly exceed the live one.
    SegmentsVersionNotIncreasing,
}

impl std::fmt::Display for GeometryRefusedReason {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            GeometryRefusedReason::SegmentsVersionNotIncreasing => f.write_str(
                "segments_version must strictly increase; a bundle swap that leaves the pin \
                 identity unchanged would answer outstanding pins against new geometry (I11)",
            ),
        }
    }
}

/// A geometry publication this engine refuses — see [`check_publishable`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct GeometryRefused {
    pub live_prefix: String,
    pub live_segments_version: u64,
    pub offered_prefix: String,
    pub offered_segments_version: u64,
    pub reason: GeometryRefusedReason,
}

impl std::fmt::Display for GeometryRefused {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "refusing to publish geometry ('{}', {}) over live ('{}', {}): {}",
            self.offered_prefix,
            self.offered_segments_version,
            self.live_prefix,
            self.live_segments_version,
            self.reason
        )
    }
}

impl std::error::Error for GeometryRefused {}

/// Whether `(prefix, segments_version)` may be published over `live` — the identity guard
/// [`crate::session::Engine::publish_geometry`] runs inside its compare-and-swap loop.
///
/// **`segments_version` must strictly increase, and this refuses rather than warns** because both
/// failures it prevents are R-wrong (this module's doc), not tuning:
///
/// - A publication that changes the *bundle* while leaving `(prefix, segments_version)` alone
///   retires nothing, and every pin naming that identity then takes [`PinManager::resolve`]'s fast
///   path and is answered against the **new** geometry — R-wrong reached through the API instead of
///   through the resolve site.
/// - Republishing an *older* `segments_version` (a rollback by pointer flip, design §10.2) is
///   refused for the mirror reason: it would put a live geometry's identity on the drain list,
///   where a later `prune_generation` would evict the live generation's own projections.
///
/// `seg_id`s are never reused across compactions or prefixes (lifecycle §5.2, contracts §2.1), so
/// strict monotonicity is what the format already promises; this only refuses to be the place it is
/// broken.
///
/// A free function rather than a method: it reads no [`PinManager`] state, and taking `&self` would
/// imply the drain list has a say in what may be published.
pub(crate) fn check_publishable(
    live: &Generation,
    prefix: &str,
    segments_version: u64,
) -> Result<(), GeometryRefused> {
    if segments_version > live.segments_version {
        return Ok(());
    }
    Err(GeometryRefused {
        live_prefix: live.prefix.clone(),
        live_segments_version: live.segments_version,
        offered_prefix: prefix.to_string(),
        offered_segments_version: segments_version,
        reason: GeometryRefusedReason::SegmentsVersionNotIncreasing,
    })
}

/// Resolves a request's presented pin (or mints one) against the live generation, within
/// lifecycle §2.2's two bounds.
pub(crate) struct PinManager {
    /// Lifecycle §2.2's TTL, in seconds. Measured from [`DrainEntry::retired_at`] and enforced at
    /// **resolve** as well as at reclaim — see this module's doc, property 3. See
    /// `tessera-server::config`'s `DEFAULT_PIN_TTL_SECS` for the page-cache argument that sizes it.
    pin_ttl_secs: u64,
    /// Lifecycle §2.2's per-session cap: the most **superseded** geometries one session may hold
    /// resolvable at once. See [`PinManager::resolve_drained`] for why the cap counts drained pins
    /// rather than minted ones.
    ///
    /// **What this does and does not bound, stated honestly.** A `DrainEntry` holds its bundle from
    /// retirement until its TTL whether or not any session ever presents it, so retention is
    /// governed entirely by `pin_ttl_secs`, [`DEFAULT_DRAIN_DEPTH_MAX`] and reclaim running — not by this.
    /// And a client can hold N drained geometries by rotating N sessions, since `Engine::authorise`
    /// mints a `token_id` per call against a cached fragment. So this is a per-session politeness
    /// limit that the spec requires and that keeps one *well-behaved* session's use of superseded
    /// geometry visible and bounded; it is not the page-cache defence. Do not reintroduce that
    /// claim here.
    ///
    /// **Must be non-zero.** At `0` every presented drained pin is a `422` instead of the `410` it
    /// should be. `tessera-server`'s config loader refuses zero, so only a direct embedder of
    /// `EngineConfig` can reach it.
    pins_per_session_max: usize,
    /// Lifecycle §2.2's ceiling — see [`DEFAULT_DRAIN_DEPTH_MAX`] for why it is configured rather
    /// than compiled in.
    drain_depth_max: usize,
    /// Superseded geometries, **oldest first**, capped at `drain_depth_max`. Depth is expected to
    /// be 0 or 1 (see [`DRAIN_DEPTH_ALARM`]), so the linear scans below are scans of a one-element
    /// vector on the cold path — a `Vec` says that, where a map would imply a size this list must
    /// never reach, and the ordering is what makes "drop the oldest" and
    /// [`PinStats::oldest_retired_secs`] expressible.
    drain: Mutex<Vec<DrainEntry>>,
    /// [`PinStats::drain_depth`], maintained under the drain lock and read without it.
    drain_depth: AtomicUsize,
    /// [`PinStats::drain_locks`]. `Relaxed` throughout: a diagnostic counter, ordered with nothing.
    drain_locks: AtomicU64,
    /// Milliseconds from [`Self::origin`] to the oldest live entry's `retired_at`, or [`NO_OLDEST`]
    /// when the list is empty. Maintained under the drain lock alongside `drain_depth`, so that
    /// [`PinStats::oldest_retired_secs`] stays lock-free; milliseconds because an `Instant` is not
    /// storable in an atomic.
    oldest_retired_ms: AtomicU64,
    /// The monotonic zero this manager measures `oldest_retired_ms` from.
    origin: Instant,
}

impl PinManager {
    pub(crate) fn new(
        pin_ttl_secs: u64,
        pins_per_session_max: usize,
        drain_depth_max: usize,
    ) -> Self {
        PinManager {
            pin_ttl_secs,
            pins_per_session_max,
            drain_depth_max,
            drain: Mutex::new(Vec::new()),
            drain_depth: AtomicUsize::new(0),
            drain_locks: AtomicU64::new(0),
            oldest_retired_ms: AtomicU64::new(NO_OLDEST),
            origin: Instant::now(),
        }
    }

    /// Resolve `presented` against the live generation, falling back to the drain list.
    ///
    /// A pin is geometry identity only — `(prefix, segments_version)` — never `overlay_version`, so
    /// an overlay swap (any accepted suppression, deletion or predicate change) does not invalidate
    /// one; only a geometry swap does. `None` mints a pin naming the live generation's geometry.
    ///
    /// **The lock discipline is part of the contract, not an implementation detail** — this
    /// module's doc, property 1. Both arms below return without touching the drain mutex; only
    /// [`Self::resolve_drained`] takes it, and only when a presented pin does not name the live
    /// generation. If a future change needs per-request state here, it needs a per-request-cheap
    /// way to keep it, not a mutex.
    ///
    /// `token_id` is the session identity the per-session cap counts against — the same
    /// process-local id the row-projection cache keys on, already in hand at the call site, and
    /// never the bearer token itself.
    pub(crate) fn resolve(
        &self,
        presented: Option<PinId>,
        token_id: u64,
        live: &Arc<Generation>,
    ) -> Result<PinnedGeometry, EngineError> {
        let Some(presented) = presented else {
            return Ok(PinnedGeometry::of_live(live));
        };
        if presented.prefix == live.prefix && presented.segments_version == live.segments_version {
            return Ok(PinnedGeometry::of_live(live));
        }
        self.resolve_drained(&presented, token_id)
    }

    /// The cold path: a presented pin that does not name the live generation. Either the drain list
    /// still carries it, or the request is `410`.
    ///
    /// **The per-session cap is enforced here, on presentation, and not at mint.** A pin naming the
    /// live generation costs nothing — it holds no bundle alive that is not already live — so it
    /// consumes no cap, and a session can hold at most one of them anyway. The resource lifecycle
    /// §2.2's cap counts is the set of *superseded* geometries a session can still resolve.
    /// Enforcing at mint would additionally have to take this lock on every request (breaking
    /// property 1) and would `422` an ordinary unpinned viewport, which is not a bound any caller
    /// could act on.
    ///
    /// **Two things this must never do, both of which look like helpfulness** and both R-wrong (see
    /// this module's doc). A restarted worker's drain list is empty, so every pre-restart pin
    /// arrives here and gets `410` — that is the specified behaviour, not a gap to close:
    ///
    /// - **Never reconstruct `(n_old, W)` from an old side-manifest.** Half-reconstruction mixes
    ///   old rows with a fresh fragment cache and a replayed overlay, in combinations only
    ///   lifecycle §3.3's build-stamp rule keeps safe — and a restart is exactly when that rule's
    ///   state is coldest.
    /// - **Never reinterpret the pin against current geometry.** It would return a viewport of
    ///   unrelated items, silently, with a `200`.
    #[cold]
    fn resolve_drained(
        &self,
        presented: &PinId,
        token_id: u64,
    ) -> Result<PinnedGeometry, EngineError> {
        let mut drain = self.lock_drain();
        let ttl = self.ttl();

        let index = drain
            .iter()
            .position(|entry| {
                entry.segments_version == presented.segments_version
                    && entry.prefix == presented.prefix
            })
            .ok_or(EngineError::PinExpired)?;
        // Past its TTL but not yet reclaimed. Refused here rather than left for the reclaim pass: a
        // bound that only bites when a background pass happens to have run is not a bound, and the
        // lifecycle thread being busy is exactly when the drain list is deepest.
        if drain[index].retired_at.elapsed() >= ttl {
            return Err(EngineError::PinExpired);
        }

        if !drain[index].holds(token_id) {
            // Over-age entries do not count: a caller must not be capped by pins it can no longer
            // present.
            let held = drain
                .iter()
                .filter(|entry| entry.retired_at.elapsed() < ttl && entry.holds(token_id))
                .count();
            if held >= self.pins_per_session_max {
                return Err(EngineError::PinCapExceeded {
                    held,
                    limit: self.pins_per_session_max,
                });
            }
            drain[index].add_holder(token_id);
        }

        let entry = &drain[index];
        Ok(PinnedGeometry {
            prefix: entry.prefix.clone(),
            segments_version: entry.segments_version,
            watermark: entry.watermark,
            bundle: Arc::clone(&entry.bundle),
        })
    }

    /// Put `superseded`'s geometry on the drain list, and trim the list to [`DEFAULT_DRAIN_DEPTH_MAX`].
    ///
    /// Returns the entries the trim removed, already verified and dropped by the same
    /// remove → verify → drop discipline [`Self::reclaim`] uses. Dropping the oldest is fail-closed:
    /// those pins `410`, and they were nearest their TTL anyway.
    ///
    /// # `live_prefix` / `live_segments_version` are an **observation**, not the caller's intent
    ///
    /// They are the geometry identity that is live *at the moment this is called* — after the
    /// caller's swap, not the identity the caller offered to that swap. Everything this method
    /// refuses follows from comparing `superseded` against that observation, and there is exactly
    /// one such comparison so that no second copy of the predicate can drift from it:
    ///
    /// - **An overlay-only swap retires nothing.** `overlay_version` moves on every accepted change
    ///   batch, and retiring there would put one drain entry per suppression on a list whose depth
    ///   alarms above 1, while holding the same `Arc<Bundle>` the live generation is still using.
    /// - **A geometry that is still live is never drained.** A caller's swap can be clobbered by a
    ///   publisher that copies `prefix` and `segments_version` forward and `store`s unconditionally
    ///   (`WritePath` does exactly that), which makes `superseded`'s geometry live again under a
    ///   *different* `Arc`. Draining it would put the live identity on the drain list, where a
    ///   later `prune_generation` would evict the live generation's own projections. Pointer
    ///   identity cannot express this and must not be used for it: the clobbering generation is a
    ///   fresh allocation.
    ///
    /// The observation is inherently a snapshot — a clobber landing after the caller took it is
    /// unobserved — so this narrows the window and does not close it. Closing it is the publisher's
    /// obligation (see `Engine::publish_geometry`).
    pub(crate) fn retire(
        &self,
        superseded: &Generation,
        live_prefix: &str,
        live_segments_version: u64,
    ) -> Vec<Reclaimed> {
        if superseded.prefix == live_prefix && superseded.segments_version == live_segments_version
        {
            return Vec::new();
        }
        let evicted: Vec<DrainEntry> = {
            let mut drain = self.lock_drain();
            // `check_publishable`'s strict monotonicity means a retired identity can never be
            // offered again, so a duplicate push is unreachable; a `debug_assert!` says that
            // without an uncoverable branch. Were it ever reachable, the push below is still the
            // conservative direction: `resolve_drained` matches the *first* entry, which is the
            // older `retired_at` and therefore the earlier expiry.
            debug_assert!(
                !drain.iter().any(|entry| {
                    entry.segments_version == superseded.segments_version
                        && entry.prefix == superseded.prefix
                }),
                "a geometry identity was retired twice: ({}, {})",
                superseded.prefix,
                superseded.segments_version
            );
            drain.push(DrainEntry {
                prefix: superseded.prefix.clone(),
                segments_version: superseded.segments_version,
                watermark: superseded.watermark,
                bundle: Arc::clone(&superseded.bundle),
                retired_at: Instant::now(),
                holders: Vec::new(),
            });
            let over = drain.len().saturating_sub(self.drain_depth_max);
            let evicted: Vec<DrainEntry> = drain.drain(..over).collect();
            self.note_depth(&drain);
            evicted
        };
        verify_and_drop(evicted)
    }

    /// One reclaim pass: **remove → verify → drop** (lifecycle §2.1).
    ///
    /// Every entry past its TTL is *first* removed from the drain list, under the lock, with no
    /// liveness consulted. Only then, with the lock released, is each removed entry's `Arc<Bundle>`
    /// checked for unique ownership, and only then is it dropped. A [`Self::resolve`] racing this
    /// pass either finds the entry (and gets a geometry whose bundle it now holds a reference to,
    /// keeping it alive for as long as it needs it) or misses it and gets `410` — which is correct,
    /// because a drained pin is an expired pin.
    ///
    /// The inverse order — verify the strong count, then remove only if it was one — is the
    /// use-after-free lifecycle §2.1 records the design review catching: a `resolve` landing between
    /// the verify and the remove clones an `Arc` the reclaimer has already decided to drop. It also
    /// never makes progress: an entry a request is holding stays on the list, so the drain list
    /// grows with every publication and the bound quietly stops existing.
    ///
    /// **This is the only thing that releases a superseded bundle's memory, and nothing in this
    /// crate calls it periodically.** The TTL bounds *availability* at `resolve`; it does not free
    /// anything. A deployment that publishes once and then goes quiescent holds the superseded
    /// bundle indefinitely at depth 1 — *at* [`DRAIN_DEPTH_ALARM`], which alarms only **above** it,
    /// so no depth gauge here signals the state. Wiring a periodic caller — the
    /// lifecycle thread, through `Engine::reclaim_pins` — is therefore a **requirement** on the
    /// stage that introduces a real publisher, not a nicety; [`PinStats::oldest_retired_secs`]
    /// exists so that its absence is visible rather than silent.
    ///
    /// **Reclamation here is the `Arc` drop and nothing else, and that is load-bearing.** Lifecycle
    /// §2.1 also has reclaim "delete retired-prefix files past their retention", and §2.2 pairs
    /// that with a durable `retired/<prefix>-<timestamp>` marker written by the *router*, because
    /// session pins span partitions and only the router knows when the last one is gone. No such
    /// marker exists yet, and it is safe for it not to exist only while nothing here deletes a
    /// file. **The day this method acquires file deletion, the marker becomes load-bearing and must
    /// land with it.**
    pub(crate) fn reclaim(&self) -> Vec<Reclaimed> {
        // 1. REMOVE.
        let removed: Vec<DrainEntry> = {
            let mut drain = self.lock_drain();
            let ttl = self.ttl();
            let all = std::mem::take(&mut *drain);
            let (expired, kept): (Vec<DrainEntry>, Vec<DrainEntry>) = all
                .into_iter()
                .partition(|entry| entry.retired_at.elapsed() >= ttl);
            *drain = kept;
            self.note_depth(&drain);
            expired
        };
        // 2. VERIFY (lock released) and 3. DROP.
        verify_and_drop(removed)
    }

    /// The operator gauges — see [`PinStats`]. Lock-free, deliberately.
    pub(crate) fn stats(&self) -> PinStats {
        let oldest = self.oldest_retired_ms.load(Ordering::Relaxed);
        let oldest_retired_secs = (oldest != NO_OLDEST).then(|| {
            let now_ms = u64::try_from(self.origin.elapsed().as_millis()).unwrap_or(u64::MAX);
            now_ms.saturating_sub(oldest) / 1_000
        });
        PinStats {
            drain_depth: self.drain_depth.load(Ordering::Relaxed),
            drain_locks: self.drain_locks.load(Ordering::Relaxed),
            oldest_retired_secs,
            pin_ttl_secs: self.pin_ttl_secs,
        }
    }

    /// Republish the lock-free gauges from the list they describe. **Called with the drain lock
    /// held**, on every path that mutates the list — a mutation that forgets this leaves
    /// [`PinStats`] describing a list that no longer exists.
    fn note_depth(&self, drain: &[DrainEntry]) {
        self.drain_depth.store(drain.len(), Ordering::Relaxed);
        let oldest = drain.first().map_or(NO_OLDEST, |entry| {
            u64::try_from(
                entry
                    .retired_at
                    .saturating_duration_since(self.origin)
                    .as_millis(),
            )
            .unwrap_or(u64::MAX)
        });
        self.oldest_retired_ms.store(oldest, Ordering::Relaxed);
    }

    fn ttl(&self) -> Duration {
        Duration::from_secs(self.pin_ttl_secs)
    }

    /// **The only place this module locks.** Counting here rather than at each call site is what
    /// makes [`PinStats::drain_locks`] a statement about the type: any new lock acquisition added
    /// to `PinManager` has to come through this function, and a property-1 regression that locks on
    /// the common path shows up in the counter rather than in a benchmark six weeks later.
    ///
    /// A poisoned mutex is recovered from rather than propagated. Nothing under this lock is a
    /// partially-applied invariant: the guarded value is a list of independent geometry
    /// descriptors, and no code inside a critical section here can panic between two writes that
    /// must both land. Refusing every subsequent request because one unrelated thread unwound would
    /// fail *closed* on availability while buying no safety.
    fn lock_drain(&self) -> MutexGuard<'_, Vec<DrainEntry>> {
        self.drain_locks.fetch_add(1, Ordering::Relaxed);
        self.drain.lock().unwrap_or_else(PoisonError::into_inner)
    }
}

/// Steps 2 and 3 of remove → verify → drop, shared by [`PinManager::reclaim`] and the depth trim in
/// [`PinManager::retire`]. **Called with the drain lock released**, both because the verify must
/// observe the world as it is rather than as it was, and because dropping a `Bundle` unmaps files
/// and must not happen inside a lock the request path can contend on.
fn verify_and_drop(entries: Vec<DrainEntry>) -> Vec<Reclaimed> {
    entries
        .into_iter()
        .map(|entry| Reclaimed {
            exclusively_held: Arc::strong_count(&entry.bundle) == 1,
            prefix: entry.prefix,
            segments_version: entry.segments_version,
        })
        .collect()
}

/// Geometry only. Overlay, buffer and `overlay_version` are deliberately ABSENT: this type is where
/// R-open (see this module's doc) is made structurally unavailable. Returning a whole `Generation`
/// instead is the natural implementation, and it is the fail-open — the request would compose
/// against a pre-suppression overlay.
///
/// [`PinManager::resolve`] can hand back state from a generation that is no longer live; this type
/// is what keeps that from including authorisation state. The drained arm builds one of these out
/// of a [`DrainEntry`], which does not carry an overlay to give.
pub(crate) struct PinnedGeometry {
    pub prefix: String,
    pub segments_version: u64,
    /// ADVISORY — status and debugging only. NEVER an input to I1 composition: the effective
    /// watermark is always the mask fragment's own (lifecycle §2.3 and its Appendix R action 2,
    /// which amended five separate phrasings implying otherwise).
    // Unread by design, hence the allow: the composition path takes its watermark from the mask
    // fragment, never from here, and the day this field acquires a reader is the day that rule
    // wants re-checking. Deleting the field instead would lose the warning above with it.
    // `a_pinned_request_composes_with_the_fragment_watermark_not_the_pinned_one` is the negative
    // control that fails if it ever does acquire one.
    #[allow(dead_code)]
    pub watermark: u64,
    pub bundle: Arc<Bundle>,
}

impl PinnedGeometry {
    fn of_live(live: &Arc<Generation>) -> Self {
        PinnedGeometry {
            prefix: live.prefix.clone(),
            segments_version: live.segments_version,
            watermark: live.watermark,
            bundle: Arc::clone(&live.bundle),
        }
    }

    /// The wire pin naming this geometry — `(prefix, segments_version)` and nothing else.
    ///
    /// Here rather than at each call site so that the day a pin gains a third component, one
    /// construction site changes instead of one per viewer verb. `watermark` is **not** part of it —
    /// contracts §2.6 makes that component advisory, and a pin the client round-trips must name
    /// geometry alone.
    pub(crate) fn pin_id(&self) -> PinId {
        PinId {
            prefix: self.prefix.clone(),
            segments_version: self.segments_version,
        }
    }
}

#[cfg(test)]
mod tests {
    //! The one branch of this module `tests/pins.rs` cannot reach: [`PinManager::retire`]'s refusal
    //! to drain a geometry that is still live.
    //!
    //! Reaching it through `Engine::publish_geometry` needs a `WritePath` `store` to land inside
    //! the window between that method's compare-and-swap and its re-read of the live generation,
    //! and no public API can schedule an interleaving that precise. Here the observation is simply
    //! passed in, which is exactly what the production call site does with whatever it observed.
    //! The observable is the one an unguarded retire gets wrong: `drain_depth`.

    use std::collections::{BTreeMap, HashMap};

    use tessera_lifecycle::{IngestBuffer, Overlay};
    use tessera_store::manifest::{IdentityDescriptor, Manifest, Quantisation};

    use super::*;

    /// An empty postings file, for the same reason the bundle above is empty: `retire` never
    /// reads one. Written to a temporary directory that is dropped immediately — the reader holds
    /// its own mapping.
    fn empty_postings() -> tessera_authz::PostingsReader {
        let dir = tempfile::TempDir::new().expect("a temp dir");
        let path = dir.path().join("postings.arrow");
        tessera_authz::write_postings(&path, &[], 32).expect("an empty postings file");
        tessera_authz::PostingsReader::open(&path, false).expect("it opens")
    }

    /// A `Generation` over an empty synthetic bundle. Nothing here reads the bundle's contents —
    /// `retire` clones the `Arc` and compares identity strings — so an empty partition map is
    /// enough, and building a real one would make this test about the fixture instead.
    fn generation(prefix: &str, segments_version: u64) -> Generation {
        let manifest = Manifest {
            bundle_format: 1,
            created_at: "2026-07-31T00:00:00Z".to_string(),
            data_plugin_hash: "builtin:passthrough:1".to_string(),
            declared_bounds: serde_json::json!({}),
            declared_scalars: vec![],
            small_term_threshold: 32,
            quantisation: Quantisation {
                x_min: 0.0,
                x_max: 1.0,
                y_min: 0.0,
                y_max: 1.0,
            },
            entity_id_high_water: 0,
            identity: IdentityDescriptor {
                construction: "siphash-2-4".to_string(),
                rounds: 1,
                key: "0123456789abcdef0123456789abcdef".to_string(),
                shard_id: 0,
                idset: 1,
            },
            slices: vec![],
            partitions: vec![],
            provenance: serde_json::json!({}),
            files: BTreeMap::new(),
        };
        Generation {
            prefix: prefix.to_string(),
            segments_version,
            watermark: 0,
            bundle: Arc::new(Bundle {
                manifest,
                partitions: HashMap::new(),
            }),
            dict: Arc::new(tessera_authz::Dict::load(&[]).expect("an empty dict needs no file")),
            postings: Arc::new(empty_postings()),
            delta_postings: Vec::new(),
            overlay_version: 0,
            overlay: Arc::new(Overlay::new()),
            buffer: Arc::new(IngestBuffer::new()),
        }
    }

    /// The control: when the observed live geometry is a *different* identity, the superseded one
    /// is drained. Without this the refusal below could pass by `retire` never draining anything.
    #[test]
    fn retire_drains_a_geometry_that_is_no_longer_live() {
        let pins = PinManager::new(300, 4, DEFAULT_DRAIN_DEPTH_MAX);
        let superseded = generation("v00000", 7);

        assert!(pins.retire(&superseded, "v00001", 8).is_empty());
        assert_eq!(
            pins.stats().drain_depth,
            1,
            "an ordinary supersession puts the outgoing geometry on the drain list"
        );
    }

    /// The guard: an observation showing `superseded`'s identity still live means the caller's swap
    /// was clobbered, and the drain list must not receive the **live** geometry's identity — a later
    /// `prune_generation` over it would evict the live generation's own projections.
    ///
    /// Distinct `Generation` values with equal identity, deliberately: that is the shape a
    /// `WritePath` store produces (it copies `prefix` and `segments_version` forward into a fresh
    /// `Arc`), and it is the shape a pointer-identity guard is blind to.
    #[test]
    fn retire_refuses_a_geometry_that_is_still_live() {
        let pins = PinManager::new(300, 4, DEFAULT_DRAIN_DEPTH_MAX);
        let superseded = generation("v00000", 7);
        let clobbered_live = generation("v00000", 7);

        let reclaimed = pins.retire(
            &superseded,
            &clobbered_live.prefix,
            clobbered_live.segments_version,
        );

        assert!(reclaimed.is_empty());
        assert_eq!(
            pins.stats().drain_depth,
            0,
            "the live geometry's identity must not reach the drain list"
        );
        assert_eq!(pins.stats().oldest_retired_secs, None);
    }

    /// The same predicate covers the overlay-only case: a swap that moves `overlay_version` alone
    /// leaves geometry identity untouched and must retire nothing, or the list would grow by one
    /// entry per accepted suppression.
    #[test]
    fn retire_ignores_an_overlay_only_swap() {
        let pins = PinManager::new(300, 4, DEFAULT_DRAIN_DEPTH_MAX);
        let superseded = generation("v00000", 7);

        assert!(pins.retire(&superseded, "v00000", 7).is_empty());
        assert_eq!(pins.stats().drain_depth, 0);
    }

    /// [`PinStats::oldest_retired_secs`] is `None` on an empty list, reports the **oldest** entry
    /// while the list is occupied, and returns to `None` when it empties.
    ///
    /// The gauge exists to make "no reclaim pass is running" visible at depth 1 — *at*
    /// [`DRAIN_DEPTH_ALARM`], which alarms only above it, so the state is invisible in every other
    /// gauge here. Its alarm condition (`oldest_retired_secs >= pin_ttl_secs` ⟺ at least one entry
    /// is over-age) is exact only because the list is ordered and this reads its front — hence the
    /// second entry and the one-second wait, without which reading the *newest* entry would pass
    /// just as well.
    #[test]
    fn oldest_retired_secs_reports_the_oldest_entry() {
        let pins = PinManager::new(300, 4, DEFAULT_DRAIN_DEPTH_MAX);
        assert_eq!(pins.stats().oldest_retired_secs, None);
        assert_eq!(pins.stats().pin_ttl_secs, 300);

        pins.retire(&generation("v00000", 7), "v00001", 8);
        assert_eq!(
            pins.stats().oldest_retired_secs,
            Some(0),
            "a freshly retired entry is zero whole seconds old"
        );

        std::thread::sleep(Duration::from_millis(1_050));
        pins.retire(&generation("v00001", 8), "v00002", 9);
        assert_eq!(pins.stats().drain_depth, 2);
        let oldest = pins
            .stats()
            .oldest_retired_secs
            .expect("the list is not empty");
        assert!(
            oldest >= 1,
            "the gauge must report the entry nearest its TTL, not the newest one; got {oldest}"
        );

        // A TTL of zero makes every entry immediately over-age, so one reclaim pass empties the
        // list — and the gauge must follow it back to `None` rather than pinning at the last value.
        let expiring = PinManager::new(0, 4, DEFAULT_DRAIN_DEPTH_MAX);
        expiring.retire(&generation("v00000", 7), "v00001", 8);
        assert_eq!(expiring.stats().oldest_retired_secs, Some(0));
        assert_eq!(expiring.reclaim().len(), 1);
        assert_eq!(expiring.stats().oldest_retired_secs, None);
        assert_eq!(expiring.stats().drain_depth, 0);
    }
}
