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
}

impl Default for TestSwitches {
    fn default() -> Self {
        TestSwitches {
            refresh_enabled: AtomicBool::new(true),
            refresh_paused: AtomicBool::new(false),
            coalesce_enabled: AtomicBool::new(true),
            merge_enabled: AtomicBool::new(true),
            fold_paused: AtomicBool::new(false),
            fold_publication_paused: AtomicBool::new(false),
            merge_publication_paused: AtomicBool::new(false),
            occupancy_stage_enabled: AtomicBool::new(true),
            serial_fallback_max_rows: AtomicU64::new(crate::viewport::SERIAL_FALLBACK_MAX_ROWS),
        }
    }
}
