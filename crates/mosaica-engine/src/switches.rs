//! The switches only a gated test hook writes; in a shipped build each holds its default for the
//! life of the process.

use std::sync::atomic::{AtomicBool, AtomicU64, AtomicU8};

pub(crate) struct TestSwitches {
    /// Whether the background refresh runs.
    pub(crate) refresh_enabled: AtomicBool,
    /// Whether the background refresh holds.
    pub(crate) refresh_paused: AtomicBool,
    /// Whether the entity-space coalesce runs.
    pub(crate) coalesce_enabled: AtomicBool,
    /// Whether the row-space merge runs.
    pub(crate) merge_enabled: AtomicBool,
    /// Whether a bulk read may be driven from its filter's matches; off, every read walks its
    /// view in stretches.
    pub(crate) driven_reads_enabled: AtomicBool,
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
    /// The zoom from which a map request is answered by the shipped scan rather than the identity
    /// bands ([`crate::bands::BANDS_BELOW_ZOOM`]).
    pub(crate) bands_below_zoom: AtomicU8,
    /// Whether a layer with no lineage tags its points from its labels; off, every layer tags them
    /// from the walk's served set, the reference the labels are tested against.
    pub(crate) tags_from_labels: AtomicBool,
    /// The fewest rows of the view one chunk of an aggregate's cell count spans.
    pub(crate) aggregate_min_chunk_rows: AtomicU64,
    /// The rows past which an aggregate's run of cells is sent as a page of its own.
    pub(crate) aggregate_alone_rows: AtomicU64,
    /// The most terms a flush may carry the dictionary to: [`tessera_authz::MAX_DISTINCT_TERMS`].
    pub(crate) max_distinct_terms: AtomicU64,
    /// How many visible items cluster centres are taken from: [`crate::slots::SAMPLE`].
    pub(crate) slot_sample: AtomicU64,
    /// Whether the next row-projection build waits, inside the build, until this is cleared.
    /// The build that takes the hold clears `projection_build_hold_wanted` and waits on
    /// `projection_build_held`, so later builds run.
    #[cfg(feature = "fault-injection")]
    pub(crate) projection_build_hold_wanted: AtomicBool,
    #[cfg(feature = "fault-injection")]
    pub(crate) projection_build_held: AtomicBool,
    /// The same pair for a level's masked-count build, held on the count pool.
    #[cfg(feature = "fault-injection")]
    pub(crate) masked_count_build_hold_wanted: AtomicBool,
    #[cfg(feature = "fault-injection")]
    pub(crate) masked_count_build_held: AtomicBool,
    /// Whether the next viewport to start drawing points waits there until this is cleared, and
    /// whether one is waiting.
    #[cfg(feature = "fault-injection")]
    pub(crate) drawing_hold_wanted: AtomicBool,
    #[cfg(feature = "fault-injection")]
    pub(crate) drawing_held: AtomicBool,
    #[cfg(feature = "fault-injection")]
    pub(crate) drawing_holding: AtomicBool,
    /// Whether the next ingest batch to pass its handler's check waits there until this
    /// is cleared, and whether one is waiting. The batch that takes the hold clears the first.
    #[cfg(feature = "fault-injection")]
    pub(crate) write_check_hold_wanted: AtomicBool,
    #[cfg(feature = "fault-injection")]
    pub(crate) write_check_held: AtomicBool,
    #[cfg(feature = "fault-injection")]
    pub(crate) write_check_holding: AtomicBool,
    /// Whether the executor waits before it drains its work queue into a commit window.
    #[cfg(feature = "fault-injection")]
    pub(crate) work_pass_paused: AtomicBool,
    /// Whether a unique declaration's round holds between finishing and handing its result back,
    /// and whether one is holding.
    #[cfg(feature = "fault-injection")]
    pub(crate) unique_round_paused: AtomicBool,
    #[cfg(feature = "fault-injection")]
    pub(crate) unique_round_holding: AtomicBool,
    /// Whether every ingest call waits on entry, on the thread its handler runs on, and how many
    /// are waiting.
    #[cfg(feature = "fault-injection")]
    pub(crate) ingest_parked: AtomicBool,
    #[cfg(feature = "fault-injection")]
    pub(crate) ingest_parked_count: AtomicU64,
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

    /// Called inside a masked-count build. Waits if a test asked for the next build to be held.
    #[cfg(feature = "fault-injection")]
    pub(crate) fn hold_masked_count_build_if_wanted(&self) {
        if self.masked_count_build_hold_wanted.swap(false, std::sync::atomic::Ordering::SeqCst) {
            while self.masked_count_build_held.load(std::sync::atomic::Ordering::SeqCst) {
                std::thread::sleep(std::time::Duration::from_millis(1));
            }
        }
    }

    /// Called by a viewport as it starts drawing points. Waits if a test asked for the next one to
    /// be held.
    #[cfg(feature = "fault-injection")]
    pub(crate) fn hold_drawing_if_wanted(&self) {
        use std::sync::atomic::Ordering;
        if self.drawing_hold_wanted.swap(false, Ordering::SeqCst) {
            self.drawing_holding.store(true, Ordering::SeqCst);
            while self.drawing_held.load(Ordering::SeqCst) {
                std::thread::sleep(std::time::Duration::from_millis(1));
            }
            self.drawing_holding.store(false, Ordering::SeqCst);
        }
    }

    /// Called by an ingest or values handler once its check has passed. Waits if a test asked for the next
    /// one to be held.
    #[cfg(feature = "fault-injection")]
    pub(crate) fn hold_write_check_if_wanted(&self) {
        use std::sync::atomic::Ordering;
        if self.write_check_hold_wanted.swap(false, Ordering::SeqCst) {
            self.write_check_holding.store(true, Ordering::SeqCst);
            while self.write_check_held.load(Ordering::SeqCst) {
                std::thread::sleep(std::time::Duration::from_millis(1));
            }
            self.write_check_holding.store(false, Ordering::SeqCst);
        }
    }

    /// Called by a unique declaration's round once it has finished. Waits while a test holds it.
    #[cfg(feature = "fault-injection")]
    pub(crate) fn hold_unique_round_if_paused(&self) {
        use std::sync::atomic::Ordering;
        self.unique_round_holding.store(true, Ordering::SeqCst);
        while self.unique_round_paused.load(Ordering::SeqCst) {
            std::thread::sleep(std::time::Duration::from_millis(1));
        }
        self.unique_round_holding.store(false, Ordering::SeqCst);
    }

    /// Called on entry to an ingest call. Waits while a test parks ingest.
    #[cfg(feature = "fault-injection")]
    pub(crate) fn park_ingest_while_wanted(&self) {
        use std::sync::atomic::Ordering;
        if !self.ingest_parked.load(Ordering::SeqCst) {
            return;
        }
        self.ingest_parked_count.fetch_add(1, Ordering::SeqCst);
        while self.ingest_parked.load(Ordering::SeqCst) {
            std::thread::sleep(std::time::Duration::from_millis(1));
        }
        self.ingest_parked_count.fetch_sub(1, Ordering::SeqCst);
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
            driven_reads_enabled: AtomicBool::new(true),
            flush_paused: AtomicBool::new(false),
            fold_paused: AtomicBool::new(false),
            fold_publication_paused: AtomicBool::new(false),
            merge_publication_paused: AtomicBool::new(false),
            occupancy_stage_enabled: AtomicBool::new(true),
            serial_fallback_max_rows: AtomicU64::new(crate::viewport::SERIAL_FALLBACK_MAX_ROWS),
            bands_below_zoom: AtomicU8::new(crate::bands::BANDS_BELOW_ZOOM),
            tags_from_labels: AtomicBool::new(true),
            aggregate_min_chunk_rows: AtomicU64::new(crate::aggregate::MIN_CHUNK_ROWS),
            aggregate_alone_rows: AtomicU64::new(crate::aggregate::ALONE_ROWS),
            max_distinct_terms: AtomicU64::new(tessera_authz::MAX_DISTINCT_TERMS),
            slot_sample: AtomicU64::new(crate::slots::SAMPLE),
            #[cfg(feature = "fault-injection")]
            projection_build_hold_wanted: AtomicBool::new(false),
            #[cfg(feature = "fault-injection")]
            projection_build_held: AtomicBool::new(false),
            #[cfg(feature = "fault-injection")]
            masked_count_build_hold_wanted: AtomicBool::new(false),
            #[cfg(feature = "fault-injection")]
            masked_count_build_held: AtomicBool::new(false),
            #[cfg(feature = "fault-injection")]
            drawing_hold_wanted: AtomicBool::new(false),
            #[cfg(feature = "fault-injection")]
            drawing_held: AtomicBool::new(false),
            #[cfg(feature = "fault-injection")]
            drawing_holding: AtomicBool::new(false),
            #[cfg(feature = "fault-injection")]
            write_check_hold_wanted: AtomicBool::new(false),
            #[cfg(feature = "fault-injection")]
            write_check_held: AtomicBool::new(false),
            #[cfg(feature = "fault-injection")]
            write_check_holding: AtomicBool::new(false),
            #[cfg(feature = "fault-injection")]
            work_pass_paused: AtomicBool::new(false),
            #[cfg(feature = "fault-injection")]
            unique_round_paused: AtomicBool::new(false),
            #[cfg(feature = "fault-injection")]
            unique_round_holding: AtomicBool::new(false),
            #[cfg(feature = "fault-injection")]
            ingest_parked: AtomicBool::new(false),
            #[cfg(feature = "fault-injection")]
            ingest_parked_count: AtomicU64::new(0),
        }
    }
}
