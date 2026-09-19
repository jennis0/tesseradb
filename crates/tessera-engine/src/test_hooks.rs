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
    /// Turn the background refresh off, so a session stays in the stale-serve window.
    ///
    /// **A test hook, and gated so it cannot exist in a shipped build.** The window decision
    /// 0044's rung 2 serves from is otherwise a race between the publication and the pool: a test
    /// that slept to catch it would assert on scheduling. `fault-injection` is the gate the
    /// integration suites already enable, and `scripts/check-layers.sh` asserts no normal
    /// dependency edge does.
    #[cfg(feature = "fault-injection")]
    #[doc(hidden)]
    pub fn set_background_refresh_for_test(&self, enabled: bool) {
        self.refresh_enabled.store(enabled, Ordering::SeqCst);
    }

    /// Turn the background occupancy fill off, so a request computes every rung itself.
    ///
    /// **A test hook, gated so it cannot exist in a shipped build**, on
    /// [`Self::set_background_refresh_for_test`]'s argument exactly. A test asserting what a
    /// *request* computed — that the anchor is the composed figure, that a suppression moves it —
    /// would otherwise be answered from a rung a background task filled, and would go on passing
    /// with the request path removed. It must be set **before** the viewport that would spawn the
    /// fill, which is any request that takes θ's anchor below `stage::BACKGROUND_DEPTH`.
    #[cfg(feature = "fault-injection")]
    #[doc(hidden)]
    pub fn set_occupancy_stage_for_test(&self, enabled: bool) {
        self.stage.enabled.store(enabled, Ordering::SeqCst);
    }

    /// How many background occupancy fills are still running — see [`crate::stage`].
    ///
    /// **A test hook**, on [`Self::set_background_refresh_for_test`]'s argument: a test that wants
    /// the filled rungs in place polls this rather than sleeping on a guess about the pool.
    #[cfg(feature = "fault-injection")]
    #[doc(hidden)]
    pub fn occupancy_stages_in_flight(&self) -> usize {
        self.stage.in_flight()
    }

    /// Drop one vocabulary's suggestion index from the live generation — the fault state
    /// [`EngineError::SuggestionUnavailable`] exists for.
    ///
    /// **A test hook, gated so it cannot exist in a shipped build**, on
    /// [`Self::set_background_refresh_for_test`]'s argument. Nothing request-shaped reaches that
    /// refusal: `Engine::open` builds an index for every vocabulary a declared category column
    /// names, and only a build that failed at open leaves one absent — which is a host condition a
    /// test cannot produce without either breaking the filesystem or reaching in here.
    ///
    /// **It submits to the executor rather than swapping the generation itself**, and that is not
    /// ceremony. The executor thread is the sole publisher (lifecycle §1.3, #59): it loads the live
    /// generation, builds a successor and stores it, so a store from any other thread can be
    /// overwritten by a swap already in flight between those two steps. A hook that lost its swap
    /// that way would leave the test asserting against an index it had asked to remove — passing or
    /// failing on timing rather than on the behaviour under test — and `scripts/check-layers.sh`
    /// refuses the second publisher for exactly that reason. Returns once the executor has
    /// published, so the caller's next request sees it.
    ///
    /// Requires a started write executor; `false` where there is none.
    #[cfg(feature = "fault-injection")]
    #[doc(hidden)]
    pub fn forget_suggestion_index_for_test(&self, vocabulary: &str) -> bool {
        self.write.forget_suggestion_index(vocabulary.to_string())
    }

    /// Rebuild one vocabulary's suggestion index from the live minter and publish it, returning
    /// once the executor has swapped — the cadence a fixture cannot otherwise reach.
    ///
    /// **A test hook, gated so it cannot exist in a shipped build**, on
    /// [`Self::forget_suggestion_index_for_test`]'s argument, and submitted through the executor
    /// for that method's reason. It exists because a rebuild is dispatched only when a side map has
    /// run 4,096 values ahead of its base — hundreds of ingest batches — and because it is the one
    /// publication that moves neither `segments_version` nor `overlay_version`, which makes it
    /// exactly the state a per-session suggestion set's key cannot see
    /// (`crate::suggest_set::SuggestSets::get`).
    ///
    /// Requires a started write executor; `false` where there is none.
    #[cfg(feature = "fault-injection")]
    #[doc(hidden)]
    pub fn rebuild_suggestion_index_for_test(&self, vocabulary: &str) -> bool {
        self.write.rebuild_suggestion_index(vocabulary.to_string())
    }

    #[cfg(feature = "fault-injection")]
    #[doc(hidden)]
    /// Hold a fold between its last pass and its submission, so a test can land a flush **inside
    /// the fold's flight** — the interleaving compaction §2's whole carry-forward table exists for,
    /// and the one nothing else here can construct.
    ///
    /// A fold over a fixture-sized corpus finishes in well under a second, so an unassisted test
    /// racing a flush against it would be a coin toss; over a real corpus the same window is hours
    /// wide and needs no help. What the hook models is therefore the *duration*, not a behaviour:
    /// the flush publishes into the old prefix exactly as it would in production, and the
    /// publication that follows sees it as a carry-forward through the live manifest, which is the
    /// only route it ever has.
    ///
    /// Held after the passes rather than during them, which is where the interleaving's
    /// consequences live: a flush landing mid-pass is one the fold's file list does not name, and
    /// so is one the publication must carry forward — which is the same state this produces.
    pub fn set_fold_paused_for_test(&self, paused: bool) {
        self.fold_paused.store(paused, Ordering::SeqCst);
    }

    /// Hold a **completed** fold in its channel, undrained, so a merge or coalesce can publish
    /// under it — see [`crate::write::MaintenanceDeps::fold_publication_paused`] for which window
    /// this is and why it is a real one.
    ///
    /// Unpausing wakes the executor, because the loop that would drain the fold may already have
    /// parked: the pause makes `publish_completed_folds` report that nothing happened, which is
    /// exactly what sends an otherwise-idle executor to `wait_for_work`.
    #[cfg(feature = "fault-injection")]
    #[doc(hidden)]
    pub fn set_fold_publication_paused_for_test(&self, paused: bool) {
        self.fold_publication_paused.store(paused, Ordering::SeqCst);
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

    /// Hold a **completed** merge in its channel, undrained, so a flush can publish **inside the
    /// merge's flight** — see [`crate::write::MaintenanceDeps::merge_publication_paused`] for the
    /// window and why it is a real one. [`Self::set_fold_publication_paused_for_test`]'s shape,
    /// including the wake: the executor draining nothing is what parks it, so unpausing must ring
    /// the doorbell.
    #[cfg(feature = "fault-injection")]
    #[doc(hidden)]
    pub fn set_merge_publication_paused_for_test(&self, paused: bool) {
        self.merge_publication_paused
            .store(paused, Ordering::SeqCst);
        if !paused {
            self.write.wake();
        }
    }

    /// Whether a completed fold is waiting, undrained, at
    /// [`Self::set_fold_publication_paused_for_test`]'s hold — the fold's half of
    /// [`Self::merge_publication_is_held_for_test`], and the condition a test waits on rather than
    /// guessing at the hold with a sleep.
    #[cfg(feature = "fault-injection")]
    #[doc(hidden)]
    pub fn fold_publication_is_held_for_test(&self) -> bool {
        self.write
            .health()
            .fold_completed_pending
            .load(Ordering::SeqCst)
    }

    /// Whether a completed merge is waiting, undrained, at
    /// [`Self::set_merge_publication_paused_for_test`]'s hold. The condition a test waits on
    /// instead of guessing at the hold with a sleep.
    #[cfg(feature = "fault-injection")]
    #[doc(hidden)]
    pub fn merge_publication_is_held_for_test(&self) -> bool {
        self.write
            .health()
            .merge_completed_pending
            .load(Ordering::SeqCst)
    }

    /// Hold the background refresh, leaving it **in flight** — the window rung 3 of
    /// `Engine::session_geometry`'s ladder sheds a racer in. Distinct from
    /// [`Self::set_background_refresh_for_test`], which models a refresh that produces nothing and
    /// *finishes*: the flag clears there, and rung 3 builds instead of refusing.
    #[cfg(feature = "fault-injection")]
    #[doc(hidden)]
    pub fn set_refresh_paused_for_test(&self, paused: bool) {
        self.refresh_paused.store(paused, Ordering::SeqCst);
    }

    /// Turn the row-space merge off — see [`Self::merge_enabled`]. Same gate, same reasoning as
    /// [`Self::set_background_refresh_for_test`].
    #[cfg(feature = "fault-injection")]
    #[doc(hidden)]
    pub fn set_merge_for_test(&self, enabled: bool) {
        self.merge_enabled.store(enabled, Ordering::SeqCst);
    }

    /// Turn the entity-space coalesce off. The soak's control needs both passes stopped, to show
    /// the axes it bounds do grow — a bound assertion against a policy that never triggers is
    /// indistinguishable from one against a policy that works.
    #[cfg(feature = "fault-injection")]
    #[doc(hidden)]
    pub fn set_coalesce_for_test(&self, enabled: bool) {
        self.coalesce_enabled.store(enabled, Ordering::SeqCst);
    }

    /// Test-only override for the serial/parallel fan-out threshold
    /// (`viewport::SERIAL_FALLBACK_MAX_ROWS`, 500,000,000 — see that constant's doc).
    /// Gated behind the `bench-timing` feature both crates' integration test suites already
    /// build with, so this does not exist at all — not even as a compiled, unreachable symbol —
    /// in a build without it, and a shipped binary never has it
    /// (`scripts/check-layers.sh` asserts the runtime gate is present and defaults closed).
    ///
    /// **Why this exists.** `SERIAL_FALLBACK_MAX_ROWS` is 500,000,000, and a fixture that
    /// genuinely clears it is impractical to build inside a unit
    /// test (real minutes even on the fast pipeline), which left the parallel branch's
    /// `pool.install` sweep — the collect-order/byte-equality claim `viewport.rs`'s module doc
    /// makes — with no test able to reach it. This is the fix: a per-`Engine` override, set once
    /// after `Engine::open` and before issuing requests, that the byte-equality tests use to force
    /// the fan-out to engage on a small, fast fixture without changing production behaviour at
    /// all. **Only the SETTER below is `bench-timing`-gated; the
    /// `serial_fallback_max_rows` field itself is present in every build and `Engine::viewport`
    /// always pays one `Relaxed` load of it** (deliberately not `#[cfg]`-gated too — two code
    /// paths in the hot path would cost auditability for the sake of one relaxed load of a value
    /// production can never write, negligible against the thousands of other atomic operations a
    /// request already does). A production build therefore always reads this field, but since
    /// nothing outside `bench-timing` can ever write it, the load always yields
    /// `SERIAL_FALLBACK_MAX_ROWS` — behaviourally identical to reading the constant directly.
    ///
    /// **Why per-`Engine`, not global or thread-local state.** `cargo test` runs tests in
    /// parallel by default, each typically constructing its own `Engine`; a process-global would
    /// have one test's override leak into another's concurrently-running assertions, and a
    /// thread-local would silently stop working the moment a request is served from a different
    /// OS thread than the one that set it (exactly what happens in `tessera-server`'s tests,
    /// where the engine is driven from `axum`/`tokio` task threads, not the test's own). Scoping
    /// the override to the `Engine` instance itself — already constructed once per test, already
    /// never shared between tests — sidesteps both hazards entirely.
    ///
    /// **Not a deployment knob.** No `tessera.toml` field reaches this; `#[doc(hidden)]` keeps it
    /// out of this crate's public docs even in a `bench-timing` build; `pub` (not `pub(crate)`) is
    /// required only because `tests/*.rs` integration tests are separate crate compilation units
    /// that cannot see `pub(crate)` items in this library crate at all.
    #[cfg(feature = "bench-timing")]
    #[doc(hidden)]
    pub fn set_serial_fallback_max_rows_for_test(&self, value: u64) {
        self.serial_fallback_max_rows
            .store(value, Ordering::Relaxed);
    }

    /// Publish a **new prefix** this process just wrote: open it, rotate the term index, the
    /// bundle identity, the fragment cache and the external-id sidecar onto it, retire `retired`,
    /// and swap — steps 5 and 6 of compaction §4, as one call.
    ///
    /// # There is no production caller, and the name now says so
    ///
    /// **The fold does not take this route.** It publishes from the executor thread, where a
    /// submission to the executor would deadlock, so it calls [`open_rotation`] — the seam both
    /// share — and publishes inline (`crate::write::Executor::publish_fold`). Nothing else writes a
    /// prefix. This entry point existed for "an embedder that wrote a prefix by some other means",
    /// which is a caller that does not exist, and its only real users are the prefix-rotation cases
    /// in `tests/prefix_rotation.rs`, which exercise the swap's half of compaction §4 against a
    /// stand-in prefix rather than a whole fold.
    ///
    /// That mattered because of what it accepts. `retired` is Rule F's executed set and **this
    /// function retires whatever it is handed** — the one place compaction §5's derivation is
    /// enforced by documentation rather than by construction, since a caller could pass any bitmap
    /// and make a deletion's tombstone leave `deleted` while its item is still visible. Keeping a
    /// `pub` name that reads like the production route, in front of that, is an invitation. The
    /// seam stays covered; what changes is that nobody reaches for this by accident.
    ///
    /// The refusals, the identity, and why the open skips verification are all [`open_rotation`]'s
    /// and documented there.
    ///
    /// # `watermark` and `dict` are the **live** values, passed through untouched
    ///
    /// Compaction §4 step 2 and pass 4. A fold folds rows; it does not advance the entity axis and
    /// it does not renumber the dictionary, so both come from the live generation and neither is
    /// derived from the fold's inputs — deriving the watermark that way moves it backwards past
    /// every entity accepted since the fold's snapshot, and every one of them goes invisible.
    /// `crate::geometry::check_publishable` refuses a regression rather than trusting this
    /// paragraph.
    ///
    /// # `retired` — Rule F, and the caller's obligation
    ///
    /// Entities whose tombstones leave `deleted` in this same swap. **Only those whose row and
    /// postings this publication demonstrably removed** — compaction §5's
    /// `{ e ∈ D₀ : no carried-forward artefact names e }`, evaluated against what was published
    /// and never against what the plan predicted, with *artefact* meaning tier, segment **and**
    /// external-id run. See `tessera_lifecycle::Overlay::retire`, which states what retiring one
    /// entity too many costs. Empty is always safe: an un-retired tombstone is fail-closed, and
    /// the next fold takes it.
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

    /// Take every projection build by `route` rather than by the one the chooser prices, or by the
    /// chosen route again on `None`.
    ///
    /// **A test hook, gated so it cannot exist in a shipped build**, on
    /// [`Self::set_background_refresh_for_test`]'s argument. What it is for is the property that
    /// the routes agree: a test that only compared chosen routes would compare one route with
    /// itself, because the chooser picks the same one for the same principal every time.
    ///
    /// A forced route with nothing to run — a split where the view has no images or the session
    /// holds no term with one, a complement over a base that does not record the row count its
    /// slots are a bijection onto, a whole-domain answer over a grant that is not whole — walks
    /// instead, and
    /// [`Self::projection_builds_by_route`] records the walk. A caller asserting that its forced
    /// route ran reads the gauge.
    #[cfg(feature = "fault-injection")]
    #[doc(hidden)]
    pub fn force_projection_route_for_test(&self, route: Option<crate::compose::ProjectionRoute>) {
        self.projection_routes.force(route);
    }

    /// The rows this session's row projection holds in `view`, built or served exactly as a
    /// viewport would reach it.
    ///
    /// **A test hook, gated so it cannot exist in a shipped build.** A row projection is a
    /// per-session cache entry behind the single-flight ladder and reaches no public surface: the
    /// answers it produces do, and a test that only compared answers could not tell a projection
    /// that lost rows from a request that was never going to return them. The suite that checks
    /// the three routes against each other compares the projections themselves, which is the
    /// claim.
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
    /// compared against.
    ///
    /// **A test hook, gated so it cannot exist in a shipped build**, on
    /// [`Self::session_projection_rows_for_test`]'s argument. It goes through `RowSpace::project`
    /// rather than through [`crate::compose::ProjectionRoute::Walk`] so that the reference is the
    /// row space's own crossing and not the same code path under another name.
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

    /// The row form this engine is **holding** for one `(view, layer, level)`, without building
    /// one — the maintained form itself, for the differential that asserts it equals a form built
    /// from scratch (`tests/artifact_bring_forward.rs`).
    ///
    /// **Test-only, and the reason is what it would otherwise be**: a caller that took the held
    /// form on a request path would be taking whatever was last written to the cache rather than
    /// the form of the generation it is serving — the freshness argument `get_or_build` makes by
    /// reading the level's version from the store it builds from.
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

    /// Drop every derived form this engine holds for one layer, so the next request builds them —
    /// the *from scratch* half of the same differential.
    #[cfg(feature = "fault-injection")]
    #[doc(hidden)]
    pub fn forget_artifact_forms_for_test(&self, layer: &str) {
        self.artifact_projections.forget(layer);
    }

    /// How many resident memberships are held on the heap rather than read through the extent
    /// that carries them — see [`tessera_lifecycle::membership::ArtifactStore::owned_memberships`].
    ///
    /// A test hook on [`Self::level_memberships_for_test`]'s argument, and a count rather than a
    /// membership: what it answers is whether a publication left anything behind.
    #[cfg(feature = "fault-injection")]
    #[doc(hidden)]
    pub fn owned_memberships_for_test(&self) -> usize {
        self.write.live().with_artifacts(|store| store.owned_memberships())
    }

    /// Every artifact of one level as `(ordinal, members, mapped)`, where `mapped` says the
    /// membership is read through the extent that carries it rather than from the heap.
    ///
    /// **A test hook, gated so it cannot exist in a shipped build**, on
    /// [`Self::set_background_refresh_for_test`]'s argument and on a second one of its own: this
    /// answers memberships in entity space with no mask anywhere near it, which is the shape no
    /// serving route may have. Where a membership lives is otherwise invisible — every reader goes
    /// through `Deref` and cannot tell the two apart — so without it the seed and the fold could
    /// stop mapping and only a memory measurement would ever say so.
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
