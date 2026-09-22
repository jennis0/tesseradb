/// When a fold is dispatched unasked. `window_min_segments` fires only inside the daily window;
/// every other threshold fires at any hour, and `None` switches its route off.
// `PartialEq` without `Eq` because two thresholds are `f64`; the derive exists for a test.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct CompactionSchedule {
    /// Seconds after the last attempt during which no route fires.
    pub min_interval_secs: u64,
    /// Seconds past midnight UTC, so daylight-saving days do not shift the window.
    pub window_start_secs: Option<u32>,
    /// Wraps past midnight. Zero never opens the window and a day or more never closes it.
    pub window_secs: u32,
    pub window_min_segments: usize,
    /// Must sit above `window_min_segments` where both are set; `tessera-server` refuses
    /// otherwise.
    pub max_segments: Option<usize>,
    pub after_deletions: Option<u64>,
    /// Retirable deletions over live rows.
    pub tombstoned_rows_fraction: Option<f64>,
    /// Dead bytes over named bytes. Reclaims space from merge churn that leaves the counts low.
    pub dead_bytes_ratio: Option<f64>,
}

impl CompactionSchedule {
    /// Every route off. `tessera-server` applies its own defaults.
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

/// Why the schedule dispatched a fold, for the log line.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum FoldTrigger {
    Window,
    SegmentCount,
    RetirableDepth,
    TombstonedRows,
    DeadBytes,
}

/// What the schedule reads at a tick. The dead-bytes gauge needs a directory walk, so [`due`]
/// takes it as a closure.
#[derive(Debug, Clone, Copy)]
pub(crate) struct Gauges {
    /// The largest across the partition's views.
    pub(crate) live_segments: usize,
    /// Deletions only, not suppressions.
    pub(crate) retirable_deletions: u64,
    /// Tombstoned rows included.
    pub(crate) live_rows: u64,
}

/// Bytes on disc under the live prefix, and bytes its manifests name.
#[derive(Debug, Clone, Copy)]
pub(crate) struct DeadBytes {
    pub(crate) on_disc: u64,
    pub(crate) named: u64,
}

/// Whether a fold is due. `last_fold_unix` is when the last attempt ended, not the last success,
/// so one bad setting cannot retry and discard at every tick. It is process-local and lost at
/// restart, which is harmless.
pub(crate) fn due(
    schedule: &CompactionSchedule,
    now_unix: u64,
    last_fold_unix: Option<u64>,
    gauges: Gauges,
    dead_bytes: impl FnOnce() -> Option<DeadBytes>,
) -> Option<FoldTrigger> {
    // Saturating, so a clock that steps backwards reads as not yet due.
    if let Some(last) = last_fold_unix {
        if now_unix.saturating_sub(last) < schedule.min_interval_secs {
            return None;
        }
    }

    // The any-hour routes come before the window so their reason is the one logged.
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
    // A ratio with a zero denominator never fires, here and for dead bytes below.
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

    // Last, and only when armed: the only gauge that needs a directory walk.
    let threshold = schedule.dead_bytes_ratio?;
    let measured = dead_bytes()?;
    let dead = measured.on_disc.saturating_sub(measured.named);
    (measured.named > 0 && dead as f64 / measured.named as f64 >= threshold)
        .then_some(FoldTrigger::DeadBytes)
}

/// Whether `now_unix` falls in `[start, start + width)` past midnight UTC, wrapping at midnight.
fn in_window(now_unix: u64, start_secs: u32, window_secs: u32) -> bool {
    const DAY: u64 = 86_400;
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

    /// The server's shipped defaults: a four-hour window from midnight UTC, 8 and 64 segments,
    /// 500,000 deletions, a day's floor.
    fn schedule() -> CompactionSchedule {
        CompactionSchedule {
            min_interval_secs: 86_400,
            window_start_secs: Some(0),
            window_secs: 4 * 3_600,
            window_min_segments: 8,
            max_segments: Some(64),
            after_deletions: Some(500_000),
            // Off, so each case has one reason to fire.
            tombstoned_rows_fraction: None,
            dead_bytes_ratio: None,
        }
    }

    /// [`due`] with no live rows and no dead-bytes walk, so neither ratio can fire.
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

    /// Seconds since the epoch at `hour`:00 UTC on day `day`.
    fn at(day: u64, hour: u64) -> u64 {
        day * 86_400 + hour * 3_600
    }

    #[test]
    fn the_windowed_route_fires_only_inside_the_window_and_only_with_work() {
        let s = schedule();
        assert_eq!(
            due_at(&s, at(10, 1), None, 8, 0),
            Some(FoldTrigger::Window),
            "inside the window"
        );
        // 63 is just under the any-hour ceiling.
        assert_eq!(
            due_at(&s, at(10, 9), None, 63, 0),
            None,
            "outside the window"
        );
        assert_eq!(
            due_at(&s, at(10, 1), None, 7, 0),
            None,
            "below the window's threshold"
        );
    }

    #[test]
    fn the_unwindowed_routes_are_not_windowed() {
        let s = schedule();
        assert_eq!(
            due_at(&s, at(10, 14), None, 1, 500_000),
            Some(FoldTrigger::RetirableDepth),
            "at the deletion limit"
        );
        assert_eq!(
            due_at(&s, at(10, 14), None, 1, 499_999),
            None,
            "below the deletion limit"
        );

        assert_eq!(
            due_at(&s, at(10, 14), None, 64, 0),
            Some(FoldTrigger::SegmentCount),
            "at the segment ceiling"
        );
        assert_eq!(
            due_at(&s, at(10, 14), None, 63, 0),
            None,
            "below the segment ceiling"
        );
    }

    #[test]
    fn the_segment_gauge_has_a_window_floor_and_an_any_hour_ceiling() {
        let s = schedule();
        assert_eq!(due_at(&s, at(10, 14), None, 8, 0), None, "8 at 14:00 fired");
        assert_eq!(
            due_at(&s, at(10, 1), None, 8, 0),
            Some(FoldTrigger::Window),
            "8 inside the window"
        );
        assert_eq!(
            due_at(&s, at(10, 1), None, 64, 0),
            Some(FoldTrigger::SegmentCount),
            "64 inside the window"
        );
    }

    #[test]
    fn the_minimum_interval_floors_both_routes_and_survives_a_backward_clock() {
        let s = schedule();
        let last = at(10, 1);
        assert_eq!(
            due_at(&s, at(10, 2), Some(last), 64, 999_999),
            None,
            "an hour after the last attempt"
        );
        assert_eq!(
            due_at(&s, at(11, 1), Some(last), 8, 0),
            Some(FoldTrigger::Window),
            "a day after the last attempt"
        );
        assert_eq!(
            due_at(&s, at(9, 1), Some(last), 64, 999_999),
            None,
            "after the clock stepped back"
        );
    }

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
            "two hours into the window"
        );
        assert_eq!(
            due_at(&s, at(11, 4), None, 8, 0),
            None,
            "past the window's end"
        );
    }

    #[test]
    fn each_route_switches_off_independently_and_off_means_never() {
        let no_window = CompactionSchedule {
            window_start_secs: None,
            ..schedule()
        };
        assert_eq!(
            due_at(&no_window, at(10, 1), None, 8, 0),
            None,
            "the window is off"
        );
        assert_eq!(
            due_at(&no_window, at(10, 1), None, 8, 500_000),
            Some(FoldTrigger::RetirableDepth),
            "deletion depth with the window off"
        );
        assert_eq!(
            due_at(&no_window, at(10, 1), None, 64, 0),
            Some(FoldTrigger::SegmentCount),
            "segment ceiling with the window off"
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
            "the ceiling is off"
        );

        assert_eq!(
            due_at(&CompactionSchedule::off(), at(10, 1), None, 1_000, u64::MAX),
            None,
            "an off schedule fired"
        );
    }

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

    /// Each ratio fires at 14:00 with one segment and deletions far below `after_deletions`.
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
            "a fifth of the rows tombstoned"
        );
        assert_eq!(
            due(&s, at(10, 14), None, healthy(9_999, 50_000), || None),
            None,
            "just under the fraction"
        );
        assert_eq!(
            due(&s, at(10, 14), None, healthy(0, 50_000), || Some(
                DeadBytes {
                    on_disc: 200,
                    named: 100
                }
            )),
            Some(FoldTrigger::DeadBytes),
            "half the bytes dead"
        );
        assert_eq!(
            due(&s, at(10, 14), None, healthy(0, 50_000), || Some(
                DeadBytes {
                    on_disc: 199,
                    named: 100
                }
            )),
            None,
            "just under the ratio"
        );
        // The ratio is dead bytes over named bytes; on disc over named would fire here.
        assert_eq!(
            due(&s, at(10, 14), None, healthy(0, 50_000), || Some(
                DeadBytes {
                    on_disc: 101,
                    named: 100
                }
            )),
            None,
            "1% dead"
        );
    }

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
        assert_eq!(walked.get(), 0, "walked inside the floor");

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
        assert_eq!(walked.get(), 0, "walked after the ceiling fired");

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
        assert_eq!(walked.get(), 1, "did not walk exactly once");

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
        assert_eq!(walked.get(), 1, "walked with the route off");
    }

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
            "a zero denominator fired"
        );
    }
}
