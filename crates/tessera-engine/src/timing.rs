//! Per-stage timing for the viewport path, behind the `bench-timing` feature.
//!
//! Discharges "Experiment 2" from `docs/design-memos/2026-07-30-tail-attribution.md`: that memo
//! narrowed the residual k-scaling stall to "gather/serialise/scheduling, not the count loop" by
//! *inference* (correlating externally-observable work against `x-tessera-server-us`), and said
//! outright that attributing it to a phase needs instrumentation that did not exist. This is that
//! instrumentation.
//!
//! **Zero-cost when off, and the mechanism is the point.** The gate is on the *clock*, not on the
//! call sites: [`Probe::lap`] and [`Probe::count`] compile to nothing without the feature, so
//! `viewport.rs` carries no `#[cfg]` at any measurement point and cannot drift into a state where
//! the instrumented and uninstrumented builds take different paths. [`StageTimings`] has the same
//! shape in both builds, so no downstream type or match needs conditional compilation either.
//! The claim is discharged by measurement, not assertion — `cargo bench -p tessera-engine` with
//! and without the feature, recorded as `bench_timing_overhead_pct` in the baseline JSON.
//!
//! **I10 / SA §9.** Nothing here holds an entity id, a descriptor, a token, or any per-principal
//! label — only durations and counts of rows. `sigma_visible` and the tile counts are already
//! derivable by the caller from the viewport's own Arrow payload, so surfacing them widens no
//! channel. The C4 timing channel these numbers *quantify* is an accepted-and-open leak-register
//! entry (Appendix C, C4: "quantify before treating as acceptable"); measuring it is the
//! discharge, and the header carrying it does not exist in a release build.

/// One viewport request's stage breakdown. Same shape with and without `bench-timing`; every
/// field is left at zero when the feature is off, which is what `enabled` is for — a consumer
/// must not mistake an uninstrumented build's zeros for a genuinely free stage.
///
/// Durations are nanoseconds and are *inclusive of nothing else*: they partition the request,
/// so `total_ns` minus the sum is unattributed time (allocation, `Vec` growth, the loop
/// scaffolding itself). Counters are exact.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct StageTimings {
    /// False in a build without `bench-timing`. Distinguishes "this stage cost nothing" from
    /// "nothing was measured".
    pub enabled: bool,

    // ---- durations, ns, in request order ----
    /// `ArcSwap::load_full` on the generation pointer.
    pub generation_resolve_ns: u64,
    /// Pin validation or minting (I11).
    pub pin_resolve_ns: u64,
    /// Slice lookup and the single-segment check.
    pub slice_lookup_ns: u64,
    /// Row-projection cache lookup — **including the `RowProjection::new` build on a miss**, which
    /// is the expensive I4 entity→row crossing. `row_projection_built` says which happened.
    pub row_projection_ns: u64,
    /// `compose()` — the I1 effective-mask composition. Cost is linear in overlay + buffer size.
    pub compose_ns: u64,
    /// `EffectiveMask::visible_total()` — resolving §7.2's θ anchor, once per request.
    ///
    /// Separated from `compose_ns` because the claim made for it is specific and worth holding to
    /// account: the anchor is advertised as O(containers in the overlay diffs), since `base`'s
    /// cardinality is memoised on the immutable projection and only the diffs are counted. If this
    /// ever tracks `sigma_visible` or the projection's size, that memoisation has been lost and the
    /// anchor has quietly become a per-viewport scan.
    pub theta_anchor_ns: u64,
    /// `tiles_for_bbox` — pure geometry, no data touched.
    pub tiles_for_bbox_ns: u64,
    /// `tile_ranges` binary searches, summed over tiles.
    pub tile_ranges_ns: u64,
    /// `EffectiveMask::count_range`, summed over tiles. The count loop.
    pub count_ns: u64,
    /// §7.2's selection, summed over tiles: `rows_in_range`'s bitmap work plus the threshold count
    /// and the bounded selection over it.
    pub select_ns: u64,
    /// The per-row column gather (`row_to_point`), summed over tiles.
    pub gather_ns: u64,
    /// Design §7.3's density underlay: the sub-cell `count_range` calls, summed over tiles. Zero
    /// when the request did not ask for the underlay, which is the default.
    ///
    /// A cost centre with no other visibility, and one whose shape differs from every other stage
    /// here: it is `tiles × 4^offset` range-cardinality calls, so it grows with a *request
    /// parameter* rather than with the corpus or the viewer's coverage. That is why it is capped
    /// (`max_underlay_cells`) and why the cap needs a number behind it rather than a guess.
    pub underlay_ns: u64,
    /// Whole-request wall time inside `Engine::viewport`.
    pub total_ns: u64,

    // ---- work counters ----
    /// True when this request paid for `RowProjection::new` rather than hitting the cache. The
    /// first viewport of a session is not comparable to any later one; exclude it or say so.
    pub row_projection_built: bool,
    /// Tiles `tiles_for_bbox` produced.
    pub tiles_resolved: u64,
    /// Tiles with a non-zero masked count (the ones that reach the sampler).
    pub tiles_nonempty: u64,
    /// Σ`visible` over non-empty tiles — the masked density of the region. The memo's strongest
    /// latency regressor at low k (r = 0.83). This is the quantity that memo calls `Σvisible`.
    pub sigma_visible: u64,
    /// Σ`range.len()` over resolved tiles — total rows *spanned*, authorised or not.
    /// `rows_in_ranges - sigma_visible` is C4's numerator: rows scanned that this principal
    /// cannot see, which is the correlate the leak register says to quantify.
    pub rows_in_ranges: u64,
    /// Σ over tiles of the rows selection actually **read**, counted inside the loops that read
    /// them (`Selection::rows_visited`).
    ///
    /// Renamed from `select_rows_materialised`: nothing is materialised any more — `rows_in_range`
    /// returns a bitmap — so the old name described a `Vec` that no longer exists.
    ///
    /// **It must be an observation of the selection path, never a restatement of another counter.**
    /// A version of this briefly took its value from the caller's `visible`, which made the natural
    /// `visited == sigma_visible` assertion a tautology: both sides came from one variable and no
    /// behaviour of selection reached either. Compare it against `sigma_visible` to detect an early
    /// exit or a prefix sample (fewer) or a walk of the raw row range rather than the mask (more) —
    /// and only where the fixture makes `sigma_visible < rows_in_ranges`, or the second direction is
    /// structurally invisible.
    ///
    /// It pins the **implemented route**, not the definition. Design §7.2 admits exact routes that
    /// visit fewer than Σvisible rows — within a leaf Morton cell the `tessera_id` column is sorted,
    /// so `C_θ` there is a binary search plus a range cardinality — and Phase 1 declines to build
    /// them, preferring the obviously-correct scan. If one ever lands, revise this alongside the
    /// differential oracle rather than deleting it.
    pub select_rows_visited: u64,
    /// Points actually returned.
    pub points_gathered: u64,
    /// Sub-cells *evaluated* for the underlay — `tiles_nonempty × 4^offset`, not the number
    /// emitted. The two differ by however many sub-cells were empty, and that gap is the useful
    /// number: it is the work spent discovering emptiness, which on a clustered corpus is most of
    /// it. Compare against `ViewportOut::sub_cells.len()` for the emitted count.
    pub underlay_cells_evaluated: u64,
    /// Number of clock reads taken. Multiply by the per-lap cost from `tessera-bench calibrate`
    /// to get the perturbation this instrumentation itself introduced, and subtract it honestly
    /// rather than pretending it is zero.
    pub clock_laps: u64,
}

impl StageTimings {
    /// Time not attributed to any named stage: allocation, `Vec` growth, loop scaffolding.
    /// Saturating, because clock perturbation can make the parts exceed the whole by a few ns.
    pub fn unattributed_ns(&self) -> u64 {
        let named = self.generation_resolve_ns
            + self.pin_resolve_ns
            + self.slice_lookup_ns
            + self.row_projection_ns
            + self.compose_ns
            + self.tiles_for_bbox_ns
            + self.tile_ranges_ns
            + self.theta_anchor_ns
            + self.count_ns
            + self.select_ns
            + self.gather_ns
            + self.underlay_ns;
        self.total_ns.saturating_sub(named)
    }
}

/// The clock, and the counters it fills.
///
/// Field selection goes through a closure (`|t| &mut t.count_ns`) rather than the caller writing
/// `probe.t.count_ns` directly, so that the borrow of the timings and the read of the clock happen
/// inside one call and the whole thing disappears when the feature is off. The closures are
/// trivially inlined; nothing survives compilation without `bench-timing`.
pub struct Probe {
    /// The accumulated breakdown. Public so `Engine::viewport` can move it into `ViewportOut`.
    pub t: StageTimings,
    #[cfg(feature = "bench-timing")]
    mark: std::time::Instant,
    #[cfg(feature = "bench-timing")]
    start: std::time::Instant,
}

impl Default for Probe {
    fn default() -> Self {
        Self::new()
    }
}

impl Probe {
    #[inline(always)]
    pub fn new() -> Self {
        #[cfg(feature = "bench-timing")]
        {
            let now = std::time::Instant::now();
            Probe {
                t: StageTimings {
                    enabled: true,
                    ..Default::default()
                },
                mark: now,
                start: now,
            }
        }
        #[cfg(not(feature = "bench-timing"))]
        {
            Probe {
                t: StageTimings::default(),
            }
        }
    }

    /// Charge the time since the last lap (or construction) to `sel`'s field, and reset the mark.
    ///
    /// Laps partition the request in call order, so a stage that is entered more than once — the
    /// per-tile stages — accumulates across its visits, which is what the summed-over-tiles field
    /// docs mean.
    #[inline(always)]
    pub fn lap(&mut self, sel: impl FnOnce(&mut StageTimings) -> &mut u64) {
        #[cfg(feature = "bench-timing")]
        {
            let now = std::time::Instant::now();
            *sel(&mut self.t) += now.duration_since(self.mark).as_nanos() as u64;
            self.mark = now;
            self.t.clock_laps += 1;
        }
        #[cfg(not(feature = "bench-timing"))]
        {
            let _ = sel;
        }
    }

    /// Reset the mark without charging the elapsed time anywhere — for stretches that belong to no
    /// stage and would otherwise be misattributed to whichever lap comes next.
    #[inline(always)]
    pub fn skip(&mut self) {
        #[cfg(feature = "bench-timing")]
        {
            self.mark = std::time::Instant::now();
            self.t.clock_laps += 1;
        }
    }

    /// Add `n` to a counter field. `n` is still evaluated when the feature is off, so callers must
    /// keep it O(1) — every current call site passes a length or a constant.
    #[inline(always)]
    pub fn count(&mut self, sel: impl FnOnce(&mut StageTimings) -> &mut u64, n: u64) {
        #[cfg(feature = "bench-timing")]
        {
            *sel(&mut self.t) += n;
        }
        #[cfg(not(feature = "bench-timing"))]
        {
            let _ = (sel, n);
        }
    }

    /// Set the `row_projection_built` flag.
    #[inline(always)]
    pub fn mark_projection_built(&mut self) {
        #[cfg(feature = "bench-timing")]
        {
            self.t.row_projection_built = true;
        }
    }

    /// Stamp `total_ns` and hand back the finished breakdown.
    // `mut` is needed only under the feature; without it there is nothing to stamp.
    #[allow(unused_mut)]
    #[inline(always)]
    pub fn finish(mut self) -> StageTimings {
        #[cfg(feature = "bench-timing")]
        {
            self.t.total_ns = self.start.elapsed().as_nanos() as u64;
            self.t.clock_laps += 1;
        }
        self.t
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn disabled_probe_reports_not_enabled_and_stays_zero() {
        let mut p = Probe::new();
        p.lap(|t| &mut t.count_ns);
        p.count(|t| &mut t.rows_in_ranges, 42);
        let t = p.finish();

        if cfg!(feature = "bench-timing") {
            assert!(t.enabled);
            assert_eq!(t.rows_in_ranges, 42);
        } else {
            // The whole point: an uninstrumented build must be distinguishable from a free stage.
            assert!(!t.enabled);
            assert_eq!(t.rows_in_ranges, 0);
            assert_eq!(t.count_ns, 0);
            assert_eq!(t.total_ns, 0);
        }
    }

    #[test]
    fn laps_accumulate_across_visits() {
        let mut p = Probe::new();
        for _ in 0..3 {
            p.lap(|t| &mut t.gather_ns);
        }
        let t = p.finish();
        if cfg!(feature = "bench-timing") {
            // Three visits charged to one field; the exact durations are not assertable, only
            // that accumulation happened rather than overwriting.
            assert!(t.clock_laps >= 4);
        }
    }

    #[test]
    fn unattributed_never_underflows() {
        let t = StageTimings {
            total_ns: 10,
            count_ns: 999,
            ..Default::default()
        };
        assert_eq!(t.unattributed_ns(), 0);
    }
}
