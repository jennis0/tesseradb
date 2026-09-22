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

#[cfg(test)]
mod tests {
    use super::*;

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
}
