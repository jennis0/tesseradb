//! The switches only a gated test hook writes; in a shipped build each holds its default for the
//! life of the process.

use std::sync::atomic::{AtomicBool, AtomicU64};

pub(crate) struct TestSwitches {
    /// Whether the background refresh runs.
    pub(crate) refresh_enabled: AtomicBool,
    /// Whether the background refresh holds.
    pub(crate) refresh_paused: AtomicBool,
    /// Whether the entity-space coalesce runs.
    pub(crate) coalesce_enabled: AtomicBool,
    /// Whether the row-space merge runs.
    pub(crate) merge_enabled: AtomicBool,
    /// Whether a flush holds between finishing on the pool and submitting the result, so it is
    /// still in flight when the next tick lands.
    pub(crate) flush_paused: AtomicBool,
    /// Whether a fold holds between finishing its passes and submitting the result.
    pub(crate) fold_paused: AtomicBool,
    /// See [`crate::Engine::set_fold_publication_paused_for_test`].
    pub(crate) fold_publication_paused: AtomicBool,
    /// See [`crate::Engine::set_merge_publication_paused_for_test`].
    pub(crate) merge_publication_paused: AtomicBool,
    /// Whether the background occupancy fill runs at all. A test turns it off so an assertion about
    /// what a *request* computed is not answered by work a background task did first.
    pub(crate) occupancy_stage_enabled: AtomicBool,
    /// The effective serial/parallel fan-out threshold every `viewport` call reads. Exists so
    /// `set_serial_fallback_max_rows_for_test` has something per-`Engine` to override.
    pub(crate) serial_fallback_max_rows: AtomicU64,
    /// Whether the next row-projection build waits, inside the build, until this is cleared.
    /// The build that takes the hold clears `projection_build_hold_wanted` and waits on
    /// `projection_build_held`, so later builds run.
    #[cfg(feature = "fault-injection")]
    pub(crate) projection_build_hold_wanted: AtomicBool,
    #[cfg(feature = "fault-injection")]
    pub(crate) projection_build_held: AtomicBool,
    /// Whether the next ingest to pass its handler's check waits there until this is cleared,
    /// and whether one is waiting. The ingest that takes the hold clears the first.
    #[cfg(feature = "fault-injection")]
    pub(crate) ingest_check_hold_wanted: AtomicBool,
    #[cfg(feature = "fault-injection")]
    pub(crate) ingest_check_held: AtomicBool,
    #[cfg(feature = "fault-injection")]
    pub(crate) ingest_check_holding: AtomicBool,
    /// Whether the executor waits before it drains its work queue into a commit window.
    #[cfg(feature = "fault-injection")]
    pub(crate) work_pass_paused: AtomicBool,
}

impl TestSwitches {
    /// Called inside a row-projection build. Waits if a test asked for the next build to be held.
    #[cfg(feature = "fault-injection")]
    pub(crate) fn hold_projection_build_if_wanted(&self) {
        // The ordering is spelled on each line: `check-layers.sh` tells an atomic from a generation
        // publication by the `Ordering::` beside the call.
        if self.projection_build_hold_wanted.swap(false, std::sync::atomic::Ordering::SeqCst) {
            while self.projection_build_held.load(std::sync::atomic::Ordering::SeqCst) {
                std::thread::sleep(std::time::Duration::from_millis(1));
            }
        }
    }

    /// Called by an ingest handler once its check has passed. Waits if a test asked for the next
    /// one to be held.
    #[cfg(feature = "fault-injection")]
    pub(crate) fn hold_ingest_check_if_wanted(&self) {
        use std::sync::atomic::Ordering;
        if self.ingest_check_hold_wanted.swap(false, Ordering::SeqCst) {
            self.ingest_check_holding.store(true, Ordering::SeqCst);
            while self.ingest_check_held.load(Ordering::SeqCst) {
                std::thread::sleep(std::time::Duration::from_millis(1));
            }
            self.ingest_check_holding.store(false, Ordering::SeqCst);
        }
    }

    /// Called by the executor before it drains its work queue. Waits while a test holds it.
    #[cfg(feature = "fault-injection")]
    pub(crate) fn hold_work_pass_if_paused(&self) {
        while self.work_pass_paused.load(std::sync::atomic::Ordering::SeqCst) {
            std::thread::sleep(std::time::Duration::from_millis(1));
        }
    }
}

impl Default for TestSwitches {
    fn default() -> Self {
        TestSwitches {
            refresh_enabled: AtomicBool::new(true),
            refresh_paused: AtomicBool::new(false),
            coalesce_enabled: AtomicBool::new(true),
            merge_enabled: AtomicBool::new(true),
            flush_paused: AtomicBool::new(false),
            fold_paused: AtomicBool::new(false),
            fold_publication_paused: AtomicBool::new(false),
            merge_publication_paused: AtomicBool::new(false),
            occupancy_stage_enabled: AtomicBool::new(true),
            serial_fallback_max_rows: AtomicU64::new(crate::viewport::SERIAL_FALLBACK_MAX_ROWS),
            #[cfg(feature = "fault-injection")]
            projection_build_hold_wanted: AtomicBool::new(false),
            #[cfg(feature = "fault-injection")]
            projection_build_held: AtomicBool::new(false),
            #[cfg(feature = "fault-injection")]
            ingest_check_hold_wanted: AtomicBool::new(false),
            #[cfg(feature = "fault-injection")]
            ingest_check_held: AtomicBool::new(false),
            #[cfg(feature = "fault-injection")]
            ingest_check_holding: AtomicBool::new(false),
            #[cfg(feature = "fault-injection")]
            work_pass_paused: AtomicBool::new(false),
        }
    }
}
