//! The engine's gated test hooks.

#[cfg(any(feature = "fault-injection", feature = "bench-timing"))]
use std::sync::atomic::Ordering;
#[cfg(feature = "fault-injection")]
use std::sync::Arc;

#[cfg(feature = "fault-injection")]
use tessera_authz::{DeltaTier, Dict};
#[cfg(feature = "fault-injection")]
use tessera_types::EntityId;

use crate::engine::Engine;
#[cfg(feature = "fault-injection")]
use crate::engine::open_rotation;
#[cfg(feature = "fault-injection")]
use crate::error::{EngineError, Result};
#[cfg(feature = "fault-injection")]
use crate::geometry::GeometryPublication;
#[cfg(feature = "fault-injection")]
use crate::session::Session;
#[cfg(feature = "fault-injection")]
use crate::write::PublishGeometryError;

impl Engine {
    /// Turn the background refresh off, so a session stays in the stale-serve window instead of
    /// racing the publication and the pool.
    #[cfg(feature = "fault-injection")]
    #[doc(hidden)]
    pub fn set_background_refresh_for_test(&self, enabled: bool) {
        self.switches.refresh_enabled.store(enabled, Ordering::SeqCst);
    }

    /// Turn the background occupancy fill off, so a request computes every rung itself, letting a
    /// test assert what the request path computed rather than what a background fill left behind.
    /// Must be set before the viewport that would spawn the fill.
    #[cfg(feature = "fault-injection")]
    #[doc(hidden)]
    pub fn set_occupancy_stage_for_test(&self, enabled: bool) {
        self.switches.occupancy_stage_enabled.store(enabled, Ordering::SeqCst);
    }

    /// How many background occupancy fills are still running, for a test to poll rather than
    /// sleep on a guess about the pool.
    #[cfg(feature = "fault-injection")]
    #[doc(hidden)]
    pub fn occupancy_stages_in_flight(&self) -> usize {
        self.stage.in_flight()
    }

    /// Drop one vocabulary's suggestion index from the live generation, producing the state
    /// [`EngineError::SuggestionUnavailable`] exists for — a host condition no request can reach.
    /// Submits through the executor, the sole publisher, rather than swapping the generation
    /// directly, so it cannot lose a race with a swap already in flight. Returns once published.
    ///
    /// Requires a started write executor; `false` where there is none.
    #[cfg(feature = "fault-injection")]
    #[doc(hidden)]
    pub fn forget_suggestion_index_for_test(&self, vocabulary: &str) -> bool {
        self.write.forget_suggestion_index(vocabulary.to_string())
    }

    /// Rebuild one vocabulary's suggestion index from the live minter and publish it, returning
    /// once the executor has swapped — the cadence a fixture cannot otherwise reach, since a
    /// rebuild is normally dispatched only after thousands of ingest batches.
    ///
    /// Requires a started write executor; `false` where there is none.
    #[cfg(feature = "fault-injection")]
    #[doc(hidden)]
    pub fn rebuild_suggestion_index_for_test(&self, vocabulary: &str) -> bool {
        self.write.rebuild_suggestion_index(vocabulary.to_string())
    }

    #[cfg(feature = "fault-injection")]
    #[doc(hidden)]
    /// Hold a fold between its last pass and its submission, so a test can land a flush inside
    /// the fold's flight — an interleaving a fixture-sized fold finishes too fast to race
    /// unassisted. The flush publishes into the old prefix exactly as in production; this only
    /// stretches the window before the fold's own publication follows.
    pub fn set_fold_paused_for_test(&self, paused: bool) {
        self.switches.fold_paused.store(paused, Ordering::SeqCst);
    }

    /// Hold a flush on the pool after it has executed, so it is still in flight when the next
    /// tick lands and the tick takes the behind-a-flush branch.
    #[cfg(feature = "fault-injection")]
    #[doc(hidden)]
    pub fn set_flush_paused_for_test(&self, paused: bool) {
        self.switches.flush_paused.store(paused, Ordering::SeqCst);
    }

    /// Whether a flush has executed and is holding at [`Self::set_flush_paused_for_test`].
    #[cfg(feature = "fault-injection")]
    #[doc(hidden)]
    pub fn flush_is_holding_for_test(&self) -> bool {
        self.write.health().flush_holding.load(Ordering::SeqCst)
    }

    /// Hold a completed fold in its channel, undrained, so a merge or coalesce can publish under
    /// it. Unpausing wakes the executor, since the pause is what parks it.
    #[cfg(feature = "fault-injection")]
    #[doc(hidden)]
    pub fn set_fold_publication_paused_for_test(&self, paused: bool) {
        self.switches.fold_publication_paused.store(paused, Ordering::SeqCst);
        if !paused {
            self.write.wake();
        }
    }

    /// Whether a fold has finished its passes and is holding at [`Self::set_fold_paused_for_test`].
    /// The condition a test waits on instead of guessing at the hold with a sleep.
    #[cfg(feature = "fault-injection")]
    #[doc(hidden)]
    pub fn fold_is_holding_for_test(&self) -> bool {
        self.write.health().fold_holding.load(Ordering::SeqCst)
    }

    /// Hold a completed merge in its channel, undrained, so a flush can publish inside the
    /// merge's flight. Unpausing wakes the executor, since the pause is what parks it.
    #[cfg(feature = "fault-injection")]
    #[doc(hidden)]
    pub fn set_merge_publication_paused_for_test(&self, paused: bool) {
        self.switches
            .merge_publication_paused
            .store(paused, Ordering::SeqCst);
        if !paused {
            self.write.wake();
        }
    }

    /// Whether a completed fold is waiting, undrained, at
    /// [`Self::set_fold_publication_paused_for_test`]'s hold, for a test to poll rather than sleep.
    #[cfg(feature = "fault-injection")]
    #[doc(hidden)]
    pub fn fold_publication_is_held_for_test(&self) -> bool {
        self.write
            .health()
            .fold_completed_pending
            .load(Ordering::SeqCst)
    }

    /// Whether a completed merge is waiting, undrained, at
    /// [`Self::set_merge_publication_paused_for_test`]'s hold, for a test to poll rather than sleep.
    #[cfg(feature = "fault-injection")]
    #[doc(hidden)]
    pub fn merge_publication_is_held_for_test(&self) -> bool {
        self.write
            .health()
            .merge_completed_pending
            .load(Ordering::SeqCst)
    }

    /// Hold the background refresh in flight, so a test can land a racer in that window.
    /// Distinct from [`Self::set_background_refresh_for_test`], which stops a refresh from
    /// running at all rather than holding one mid-flight.
    #[cfg(feature = "fault-injection")]
    #[doc(hidden)]
    pub fn set_refresh_paused_for_test(&self, paused: bool) {
        self.switches.refresh_paused.store(paused, Ordering::SeqCst);
    }

    /// Hold the next row-projection build open, inside the build, until
    /// [`Self::release_projection_build_for_test`]. Builds that start after it are not held.
    #[cfg(feature = "fault-injection")]
    #[doc(hidden)]
    pub fn hold_next_projection_build_for_test(&self) {
        self.switches
            .projection_build_held
            .store(true, Ordering::SeqCst);
        self.switches
            .projection_build_hold_wanted
            .store(true, Ordering::SeqCst);
    }

    #[cfg(feature = "fault-injection")]
    #[doc(hidden)]
    pub fn release_projection_build_for_test(&self) {
        self.switches
            .projection_build_held
            .store(false, Ordering::SeqCst);
    }

    /// Turn the row-space merge off.
    #[cfg(feature = "fault-injection")]
    #[doc(hidden)]
    pub fn set_merge_for_test(&self, enabled: bool) {
        self.switches.merge_enabled.store(enabled, Ordering::SeqCst);
    }

    /// Turn the entity-space coalesce off, so a soak can show the axes it bounds keep growing
    /// with the pass stopped.
    #[cfg(feature = "fault-injection")]
    #[doc(hidden)]
    pub fn set_coalesce_for_test(&self, enabled: bool) {
        self.switches.coalesce_enabled.store(enabled, Ordering::SeqCst);
    }

    /// Override the serial/parallel fan-out threshold (`viewport::SERIAL_FALLBACK_MAX_ROWS`),
    /// so a small fixture can force the parallel branch to engage without changing production
    /// behaviour — the real threshold is too high to clear inside a unit test.
    ///
    /// Scoped to the `Engine` instance rather than global or thread-local state, since tests run
    /// in parallel each with their own `Engine`, and the engine is driven from a different OS
    /// thread than the one that set this in `tessera-server`'s own tests.
    ///
    /// The `serial_fallback_max_rows` field itself is present in every build and always read;
    /// only this setter is gated, so nothing outside `bench-timing` can ever write a value other
    /// than the constant.
    #[cfg(feature = "bench-timing")]
    #[doc(hidden)]
    pub fn set_serial_fallback_max_rows_for_test(&self, value: u64) {
        self.switches
            .serial_fallback_max_rows
            .store(value, Ordering::Relaxed);
    }

    /// Publish a new prefix this process just wrote: open it, rotate the term index, the bundle
    /// identity, the fragment cache and the external-id sidecar onto it, retire `retired`, and
    /// swap.
    ///
    /// No production caller: a fold publishes from the executor thread instead, calling
    /// [`open_rotation`] directly and inline, since submitting to the executor from the executor
    /// would deadlock. This method exists for the prefix-rotation tests, which exercise the swap
    /// against a stand-in prefix rather than a whole fold.
    ///
    /// `retired` is retired exactly as handed to it, with no check that it matches what this
    /// publication actually removed — passing the wrong set here would leave a deletion's
    /// tombstone in place while its item is still visible. `watermark` and `dict` are passed
    /// through untouched from the live generation: deriving them from the fold's inputs instead
    /// would move the watermark backwards past every entity accepted since the fold's snapshot,
    /// making each of them invisible. Empty `retired` is always safe.
    #[cfg(feature = "fault-injection")]
    #[doc(hidden)]
    pub fn publish_rotated_prefix_for_test(
        &self,
        prefix: &str,
        segments_version: u64,
        watermark: u64,
        dict: Arc<Dict>,
        delta_postings: Vec<Arc<DeltaTier>>,
        retired: &[EntityId],
    ) -> std::result::Result<(), PublishGeometryError> {
        let mut retired_bitmap = croaring::Bitmap::new();
        for entity in retired {
            retired_bitmap.add(u32::try_from(entity.raw()).expect(
                "entity ids are capped at u32::MAX by the I9 allocator (contracts §2.6 r6)",
            ));
        }
        let (bundle, rotation) = open_rotation(
            &self.bundle_root,
            prefix,
            &self.generation.load().fragments,
            retired_bitmap,
        )?;

        self.write.publish_geometry(
            GeometryPublication::within_prefix(
                prefix.to_string(),
                segments_version,
                watermark,
                bundle,
                dict,
                delta_postings,
            )
            .rotating(rotation),
        )
    }

    /// Force every projection build to take `route` rather than the one the chooser prices, or
    /// the chosen route again on `None`, so a test can compare routes that agree rather than
    /// comparing the chooser's pick with itself. A forced route with nothing to run walks instead,
    /// and [`Self::projection_builds_by_route`] records the walk for a caller to assert on.
    #[cfg(feature = "fault-injection")]
    #[doc(hidden)]
    pub fn force_projection_route_for_test(&self, route: Option<crate::projection::ProjectionRoute>) {
        self.projection_routes.force(route);
    }

    /// The rows this session's row projection holds in `view`, built or served exactly as a
    /// viewport would reach it — read directly since a row projection reaches no public surface
    /// on its own, only the answers built from it, which could not tell a projection that lost
    /// rows from a request that was never going to return them.
    #[cfg(feature = "fault-injection")]
    #[doc(hidden)]
    pub fn session_projection_rows_for_test(
        &self,
        session: &Session,
        view: &str,
    ) -> Result<croaring::Bitmap> {
        let generation = self.generation.load_full();
        let view_data = generation
            .bundle
            .partitions
            .values()
            .find_map(|partition| partition.views.get(view))
            .ok_or_else(|| EngineError::UnknownView(view.to_string()))?;
        let mut probe = crate::timing::Probe::new();
        let geometry =
            self.session_geometry(session, &generation, view, view_data, &None, &mut probe)?;
        Ok(geometry.projection.bitmap().clone())
    }

    /// The same rows by the walk, built here and cached nowhere — the reference every route is
    /// compared against. Goes through `RowSpace::project` directly rather than through
    /// [`crate::projection::ProjectionRoute::Walk`], so the reference is not the same code path
    /// under another name.
    #[cfg(feature = "fault-injection")]
    #[doc(hidden)]
    pub fn session_walk_rows_for_test(
        &self,
        session: &Session,
        view: &str,
    ) -> Result<croaring::Bitmap> {
        let generation = self.generation.load_full();
        let view_data = generation
            .bundle
            .partitions
            .values()
            .find_map(|partition| partition.views.get(view))
            .ok_or_else(|| EngineError::UnknownView(view.to_string()))?;
        let fragment = self.fragment_for(session, &generation)?;
        let mut rows = view_data.row_space.project(&fragment.view());
        rows.run_optimize();
        Ok(rows)
    }

    /// The row form this engine is holding for one `(view, layer, level)`, without building one —
    /// for a differential that asserts it equals a form built from scratch. On a request path
    /// this would serve whatever was last cached rather than the generation being served, which
    /// is why no non-test caller reads it this way.
    #[cfg(feature = "fault-injection")]
    #[doc(hidden)]
    pub fn held_artifact_form_for_test(
        &self,
        view: &str,
        layer: &str,
        level: u32,
    ) -> Option<std::sync::Arc<crate::artifacts::ArtifactRows>> {
        self.artifact_projections.held_form(view, layer, level)
    }

    /// Drop every derived form this engine holds for one layer, so the next request builds them
    /// from scratch.
    #[cfg(feature = "fault-injection")]
    #[doc(hidden)]
    pub fn forget_artifact_forms_for_test(&self, layer: &str) {
        self.artifact_projections.forget(layer);
    }

    /// How many resident memberships are held on the heap rather than read through the extent
    /// that carries them, for a test to check whether a publication left anything behind.
    #[cfg(feature = "fault-injection")]
    #[doc(hidden)]
    pub fn owned_memberships_for_test(&self) -> usize {
        self.write.live().with_artifacts(|store| store.owned_memberships())
    }

    /// Every artifact of one level as `(ordinal, members, mapped)`, where `mapped` says the
    /// membership is read through the extent that carries it rather than from the heap. Answers
    /// memberships in entity space with no mask, a shape no serving route may use: every reader
    /// otherwise goes through `Deref` and cannot tell heap-held from mapped apart, so without
    /// this a form that stopped mapping would show up only in a memory measurement.
    #[cfg(feature = "fault-injection")]
    #[doc(hidden)]
    pub fn level_memberships_for_test(
        &self,
        layer: &str,
        level: u32,
    ) -> Vec<(u32, Vec<u32>, bool)> {
        self.write.live().with_artifacts(|store| {
            store
                .level(layer, level)
                .map(|(ordinal, record)| {
                    (
                        ordinal,
                        record.members.iter().collect::<Vec<u32>>(),
                        record.members.is_mapped(),
                    )
                })
                .collect()
        })
    }
}
