//! The engine's operator gauges and its cache and limit setters.

use std::sync::atomic::Ordering;

use tessera_types::TermId;

#[cfg(doc)]
use crate::Generation;
use crate::engine::Engine;

/// One (partition, view)'s live segment count — [`Engine::live_segment_counts`]'s element, and
/// what `/control/status` publishes under `segments`.
///
/// **Plain `String`s and a `usize`, defined here rather than re-exported from `tessera-store`.**
/// `check-layers.sh` denies a `tessera-server → tessera-store` edge (SA §3), so a gauge the server
/// publishes must be nameable from this crate — the discipline `FragmentCacheStats` and
/// `DeclaredScalar` already establish at the crate root. This one owns nothing of the store's
/// vocabulary, so it is a definition here rather than a re-export.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ViewSegments {
    pub partition: String,
    pub view: String,
    /// Segments this view's viewport sweep would iterate — base plus every flush extent merge has
    /// not yet collapsed.
    pub segments: usize,
}

/// One partition's live geometry position — [`Engine::partition_status`]'s element, and what
/// `/control/status` publishes as contracts §3.4's per-partition block.
///
/// Defined here rather than re-exported from `tessera-store`, for [`ViewSegments`]' reason: the
/// server may not depend on the store (SA §3), so a value it publishes must be nameable from this
/// crate.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PartitionStatus {
    pub partition: String,
    /// The geometry version: bumped by flush, merge and fold publications, never by an
    /// overlay/buffer update — the [`Generation`] field of the same name.
    pub segments_version: u64,
    /// The highest entity id folded into published row geometry — moved by flush, held still by
    /// merge and fold.
    pub watermark: u64,
}

impl Engine {
    /// How many requests were served from a one-generation-stale entry (decision 0044's rung 2),
    /// and how many entries the background refresh has produced. Read together: a deployment where
    /// the second rises and [`Self::full_projection_builds`] does not is one where the refresh is
    /// keeping up with the tick.
    pub fn stale_serves(&self) -> u64 {
        self.stale_serves.load(Ordering::Relaxed)
    }

    /// See [`Self::stale_serves`].
    pub fn refreshes(&self) -> u64 {
        self.refreshes.load(Ordering::Relaxed)
    }

    /// Filtered viewports served by each crossing route, `(projected, per_tile)` — see
    /// [`Self::filter_crossings_projected`]'s doc for why this is worth watching. Unfiltered
    /// requests cross nothing and are counted in neither.
    pub fn filter_crossing_routes(&self) -> (u64, u64) {
        (
            self.filter_crossings_projected.load(Ordering::Relaxed),
            self.filter_crossings_per_tile.load(Ordering::Relaxed),
        )
    }

    /// Filtered viewports that evaluated in row space (decision 0068) — see
    /// [`Self::filter_row_routed`]'s doc. Disjoint from neither crossing counter: a mixed tree
    /// counts here *and* in whichever crossing its entity sub-trees took.
    pub fn filter_row_routes(&self) -> u64 {
        self.filter_row_routed.load(Ordering::Relaxed)
    }

    /// `member_of` leaves served by the row-column walk rather than by the artifact-major
    /// membership — see [`Self::member_of_column_walks`].
    pub fn member_of_column_walks(&self) -> u64 {
        self.member_of_column_walks.load(Ordering::Relaxed)
    }

    /// Delegates to `WritePath::allocator_high_water`, which owns the allocator; see that
    /// method's doc.
    pub fn allocator_high_water(&self) -> u64 {
        self.write.live().allocator_high_water()
    }

    /// The masked-count cache's gauges — see [`crate::histogram::MaskedCountStats`]. Operator plane
    /// only; a count of structures, naming no artifact and no principal.
    pub fn masked_count_cache_stats(&self) -> crate::histogram::MaskedCountStats {
        self.masked_counts.stats()
    }

    /// The derived-geometry cache's gauges — see [`crate::derived_cache::DerivedCacheStats`].
    /// Operator plane only; a count of structures, naming no artifact and no principal.
    ///
    /// `hit_rate` is the figure this cache is judged on, and it is a figure about a *pan*: the same
    /// principal panning across one layer re-serves mostly the same artifacts, which is what makes
    /// a held shape worth its bytes.
    pub fn derived_cache_stats(&self) -> crate::derived_cache::DerivedCacheStats {
        self.derived_geometry.stats()
    }

    /// Bound the derived-geometry cache. An embedder that never calls this gets
    /// `crate::derived_cache`'s own default, which is where the figure is argued — there is no
    /// configuration key, because an entry's size is bounded by the vertex budget rather than by
    /// the corpus.
    pub fn set_derived_cache_bytes(&self, bytes: u64) {
        self.derived_geometry.set_bound_bytes(bytes);
    }

    /// How many levels are recorded row-major and served artifact-major — see
    /// [`crate::artifacts::ArtifactProjections::layout_fallbacks`].
    pub fn layout_fallbacks(&self) -> u64 {
        self.artifact_projections.layout_fallbacks()
    }

    /// How many fold-written row-major columns were claimed rather than composed.
    pub fn columns_adopted(&self) -> u64 {
        self.artifact_projections.columns_adopted()
    }

    /// How many row-major columns were composed from a level's row form rather than claimed — see
    /// [`crate::artifacts::ArtifactProjections::columns_composed`].
    pub fn columns_composed(&self) -> u64 {
        self.artifact_projections.columns_composed()
    }

    /// The serving layout recorded for one `(layer, level)`, or `None` where no such layer is
    /// registered. Operator plane only: it names no artifact and no principal, and nothing on the
    /// wire carries it.
    pub fn recorded_layout(
        &self,
        layer: &str,
        level: u32,
    ) -> Option<tessera_types::layer::ServingLayout> {
        self.write
            .live()
            .registered_layer(layer)
            .map(|registered| registered.layout_of(level))
    }

    /// Bound both caches, and the only route by which the two config keys reach them.
    ///
    /// Called by `tessera_server::prepare` immediately after [`Self::open`], *after* it has
    /// validated both figures against `expected_concurrent_sessions`. An embedder that never calls
    /// this gets unbounded caches.
    pub fn set_cache_bounds(&self, row_projection_bytes: u64, fragment_bytes: u64) {
        self.row_projection_cache
            .set_bound_bytes(row_projection_bytes);
        self.generation
            .load()
            .fragments
            .set_memory_bound(fragment_bytes);
    }

    /// Bound the masked-count cache (`serve.masked_count_cache_bytes`).
    ///
    /// **Its own setter rather than a third argument to [`Self::set_cache_bounds`]**, because the
    /// two callers are different: every embedder calls that one through `tessera_server::prepare`,
    /// and this key exists for a deployment that has a row-major layer at all — which is a property
    /// of the corpus rather than of the box. An embedder that never calls it gets an unbounded
    /// cache, which is what a read-only embedder over a small corpus wants.
    pub fn set_masked_count_cache_bytes(&self, bytes: u64) {
        self.masked_counts.set_bound_bytes(bytes);
    }

    /// Bound the region decomposition cache (`serve.region_cache_bytes`) — a setter for
    /// [`Self::set_masked_count_cache_bytes`]'s reason.
    pub fn set_region_cache_bytes(&self, bytes: u64) {
        self.region_cache.set_bound_bytes(bytes);
    }

    /// `serve.max_region_cells` — the boundary-cell budget a region's descent stops at
    /// (selection-operand §6; published on `/v1/meta`). A setter rather than an `EngineConfig`
    /// field, for [`Self::set_masked_count_cache_bytes`]'s reason; the default is
    /// [`crate::region::DEFAULT_MAX_REGION_CELLS`].
    pub fn set_max_region_cells(&self, cells: usize) {
        self.max_region_cells.store(cells as u64, Ordering::Relaxed);
    }

    /// The region cache's gauges, beside the row-projection cache's.
    pub fn region_cache_stats(&self) -> crate::single_flight::CacheStats {
        self.region_cache.stats()
    }

    /// The occupancy memo's gauges — one entry per `(session, view, depth, generation)` rung of
    /// θ's `N_occ` ladder. Operator plane only; a count of structures, naming no principal.
    ///
    /// `evictions` rising is the memo doing what its bound is for: the entries it removes are
    /// rungs taken against a superseded generation, which no request can ask for again.
    pub fn occupancy_cache_stats(&self) -> crate::single_flight::CacheStats {
        self.occupancy.stats()
    }

    /// Bound the occupancy memo. An embedder that never calls this gets
    /// [`crate::occupancy::DEFAULT_OCCUPANCY_CACHE_BYTES`], which is where the figure is argued.
    /// A setter rather than an `EngineConfig` field, for [`Self::set_masked_count_cache_bytes`]'s
    /// reason.
    pub fn set_occupancy_cache_bytes(&self, bytes: u64) {
        self.occupancy.set_bound_bytes(bytes);
    }

    /// How long a request parks on another request's in-flight row-projection build before it is
    /// refused (`serve.single_flight_wait_ms`, decision 0058).
    ///
    /// An embedder that never calls this gets `single_flight::DEFAULT_WAIT_BUDGET_MS`, which is
    /// argued from the measured build cost it has to outlast rather than being a placeholder.
    pub fn set_single_flight_wait_ms(&self, wait_budget_ms: u64) {
        self.row_projection_cache.set_wait_budget_ms(wait_budget_ms);
    }

    /// The row count at which a commit window closes (`ingest.commit_window_max_items`,
    /// which counts **rows** — see that key's doc).
    ///
    /// **A setter rather than a `start_write_executor` argument**, on
    /// [`Engine::set_overlay_soft_limit`]'s precedent: a knob every embedder and every test would
    /// otherwise have to pass explicitly is a knob that gets passed wrong.
    ///
    /// An embedder that never calls this gets `write::DEFAULT_COMMIT_WINDOW_MAX_ROWS`, which is a
    /// real bound and deliberately not "unbounded": the drain that fills a window frees a
    /// bounded-queue slot per entry, so a window bounded only by "the queue is empty" is bounded by
    /// nothing under sustained load. **There is no unset value and no "off" for this knob** — unlike
    /// the soft limit below, a `usize::MAX` here is an unbounded window — the failure this bound
    /// exists to prevent, not a disabled feature. `0` is clamped to `1` (the
    /// documented spelling for *no* grouping) rather than accepted as "close at zero rows", and
    /// `tessera-server`'s config refuses it outright.
    pub fn set_commit_window_max_rows(&self, rows: usize) {
        self.write.health().set_commit_window_max_rows(rows);
    }

    /// The overlay depth at which the executor raises an alarm.
    ///
    /// **A setter rather than a `start_write_executor` argument**, on [`Engine::set_cache_bounds`]'
    /// precedent and for the same reason.
    ///
    /// **The predicate is evaluated once, here, as well as on every later deny apply.** The
    /// executor's check covers the only place the overlay grows *at runtime*, but a WAL replay
    /// builds an overlay before any executor exists (`WritePath::reconstruct`), so a node
    /// restarting with more suppressions than the limit would otherwise be over it from its first
    /// instruction with the alarm counter at zero, and silent until the next deny arrived.
    ///
    /// `usize::MAX` is the unset value and disables the alarm; `tessera-server`'s config refuses
    /// `0`, so the two sides of the boundary never disagree about what "off" means.
    pub fn set_overlay_soft_limit(&self, limit: usize) {
        self.write.health().set_overlay_soft_limit(limit);
        let depth = self.overlay_depth();
        // Same edge trigger as the executor's, through the same function: setting the limit re-arms
        // it, so a limit landing under a live overlay alarms exactly once here and the next deny
        // apply does not repeat it.
        if self.write.health().note_overlay_depth(depth) {
            tracing::warn!(
                overlay_depth = depth,
                overlay_soft_limit = limit,
                "ALARM: this node replayed a WAL whose overlay is already at or above the \
                 configured soft limit. A fold is what brings it down, and the schedule's \
                 retirable-depth route dispatches one at this threshold by default — so this is a \
                 signal that the fold has work, not that nothing will act (compaction §9)"
            );
        }
    }

    /// The live overlay's entry count — the gauge `/control/status` publishes beside the soft
    /// limit's alarm counter. Read straight off the current generation, so it needs no counter of
    /// its own and cannot drift from what a request would compose against.
    pub fn overlay_depth(&self) -> usize {
        self.generation.load().overlay.len()
    }

    /// Rows the bundle's segments hold, tombstoned ones included — compaction §9's denominator, and
    /// the only figure on `/control/status` that says how large the corpus actually is.
    pub fn live_rows(&self) -> u64 {
        self.generation
            .load()
            .bundle
            .partitions
            .values()
            .flat_map(|partition| partition.manifest.segments.iter())
            .map(|descriptor| u64::from(descriptor.row_count))
            .sum()
    }

    /// Retirable deletions — `|deleted|`, never the union with `suppressed`.
    ///
    /// **Beside [`Engine::overlay_depth`] rather than instead of it, and the pair is the point.**
    /// Depth is what an operator alarms on and what `overlay_soft_limit` bounds; this is what a fold
    /// can actually *reduce*, since Rule S says a suppression never retires. A deployment holding
    /// half a million standing suppressions has a deep overlay and nothing for a fold to do, and
    /// only publishing both numbers makes that legible (compaction §9).
    pub fn retirable_deletions(&self) -> u64 {
        self.generation.load().overlay.deleted_len()
    }

    /// Live segments per (partition, view), read straight off the current generation — the gauge
    /// decision 0049 obliges and `/control/status` publishes as `segments`.
    ///
    /// **This is a read-path constant made observable, not a maintenance counter.** A viewport pays
    /// a measured 1.4–1.6 µs per (tile × segment), and `MergePolicy::select`'s ladder saturates at
    /// `max_merged_segment_bytes` — so live segment count settles at corpus bytes ÷ the saturation
    /// size and thereafter tracks the corpus rather than being bounded by merge (decision 0049,
    /// pinned by `merge_selection.rs`'s
    /// `the_size_ladder_saturates_at_the_cap_and_segment_count_then_tracks_the_corpus`). At 10⁹ rows
    /// that is ~152 segments and ~73 ms on a 300-tile viewport against a 135–164 ms baseline. It is
    /// invisible at 10⁷, which is why nothing measured it until the corpus was large enough, and why
    /// it needs a gauge rather than a soak.
    ///
    /// **Off the live generation, with no counter of its own**, for `overlay_depth`'s reason: a
    /// separate counter maintained by flush and merge is a second definition that can drift from the
    /// segment set a request actually sweeps. What a reader gets here is exactly what
    /// `viewport::tile_ranges_all` would iterate at the same instant.
    ///
    /// Sorted by `(partition, view)` because the generation holds them in `HashMap`s: an operator
    /// diffing two status responses must not see a reordering that means nothing.
    pub fn live_segment_counts(&self) -> Vec<ViewSegments> {
        let generation = self.generation.load();
        let mut counts: Vec<ViewSegments> = generation
            .bundle
            .partitions
            .iter()
            .flat_map(|(partition, data)| {
                data.views
                    .iter()
                    .map(move |(view, view_data)| ViewSegments {
                        partition: partition.clone(),
                        view: view.clone(),
                        segments: view_data.segments.len(),
                    })
            })
            .collect();
        counts.sort_by(|a, b| (&a.partition, &a.view).cmp(&(&b.partition, &b.view)));
        counts
    }

    /// Each partition's live `(segments_version, watermark)` — the per-partition status block of
    /// contracts §3.4, and the stage barrier correctness-suite §12.3 reads: a version bump is how
    /// a driver knows a flush, merge or fold published, and the watermark is how it knows which
    /// entities the published geometry covers.
    ///
    /// **One generation load for the whole vector**, so the version and the watermark agree with
    /// each other — two loads could straddle a publication and pair a new version with an old
    /// watermark. Both scalars live on the generation rather than per partition: this build
    /// publishes one partition (`tessera-build` writes exactly one), so the generation's pair *is*
    /// that partition's pair. A multi-partition deployment flushes partitions independently
    /// (contracts §0.3 deviation 4), so partitioning's arrival moves these two fields onto
    /// per-partition state — the vector shape here is what keeps that a value change rather than
    /// a second status surface.
    pub fn partition_status(&self) -> Vec<PartitionStatus> {
        let generation = self.generation.load();
        let mut rows: Vec<PartitionStatus> = generation
            .bundle
            .partitions
            .keys()
            .map(|partition| PartitionStatus {
                partition: partition.clone(),
                segments_version: generation.segments_version,
                watermark: generation.watermark,
            })
            .collect();
        // Sorted for `live_segment_counts`' reason: the map's order means nothing, and an operator
        // diffing two status responses must not see a reordering that means nothing either.
        rows.sort_by(|a, b| a.partition.cmp(&b.partition));
        rows
    }

    /// The row-projection cache's operator gauges — what `/control/status` publishes as
    /// `projection_cache`. The fragment tier's twin is [`Self::fragment_cache_stats`].
    pub fn row_projection_cache_stats(&self) -> crate::single_flight::CacheStats {
        self.row_projection_cache.stats()
    }

    /// The fragment cache's operator gauges — the authz-tier twin of
    /// [`Self::row_projection_cache_stats`]. `tessera_engine::FragmentCacheStats` is the name a
    /// caller outside this crate should use for the return type: `tessera-server` may not depend on
    /// `tessera-authz` (SA §3, enforced by `scripts/check-layers.sh`), and the type is not a public
    /// path at that crate's root anyway.
    ///
    /// # Four narrow methods, not one `&Arc<FragmentCache>`
    ///
    /// This and the three below replace a `fragment_cache()` accessor that handed out the whole
    /// cache. The needs are `stats`, `evict`, `canonical_key_for` and `rebuild_count`; what came
    /// with them was `FragmentCache::set_memory_bound` — **a public knob that silently undoes the
    /// bound `tessera_server::prepare`'s startup refusal exists to enforce**, reachable from any
    /// holder of an `&Engine`. `crate::pins`' re-export argues exactly this discipline ("what
    /// escapes is only what a caller outside this crate genuinely needs"). The bound is set once,
    /// through
    /// [`Self::set_cache_bounds`], by the one caller that has validated it.
    pub fn fragment_cache_stats(&self) -> tessera_authz::fragment::CacheStats {
        self.generation.load().fragments.stats()
    }

    /// Times the fragment cache has actually re-unioned postings (rather than reopening a
    /// digest-verified `.frag` sidecar or hitting the in-memory tier). The observable that
    /// separates an in-memory eviction from a genuinely cold rebuild.
    ///
    /// **This and [`Self::fragment_cache_stats`] are read off the live generation's cache, so both
    /// reset to zero at a compaction.** That is not a lost counter: a fold rotates the bundle
    /// identity, and the entries counted before it are keyed under an identity nothing will compute
    /// again — a hit rate carried across would be describing two different caches as one. An
    /// operator watching a fold sees the numbers restart, which is the honest reading.
    pub fn fragment_cache_rebuilds(&self) -> u64 {
        self.generation.load().fragments.rebuild_count()
    }

    /// The canonical cache key for `satisfied` under this engine's bundle and plugin identity, at
    /// the live generation's watermark — the only way to name a fragment entry from outside, and
    /// therefore what [`Self::evict_fragment`] takes. Pure; reveals nothing the caller did not
    /// supply.
    ///
    /// The watermark is the live one rather than a parameter because an entry a caller could want
    /// to name is one the current generation could produce; a stale-watermark entry is unreachable
    /// by any lookup anyway (§9).
    pub fn fragment_canonical_key(&self, satisfied: &[TermId]) -> [u8; 32] {
        let generation = self.generation.load();
        generation
            .fragments
            .canonical_key_for(satisfied, generation.watermark)
    }

    /// Drop one entry from the fragment cache's **in-memory** tier; returns whether it was there.
    /// The digest-verified `.frag`/`.meta` pair is deliberately left on disk — see
    /// `FragmentCache::evict`, which carries that argument and the caveat that a live `Session`
    /// holding the fragment keeps its mapping alive regardless.
    ///
    /// The conformance command is the intended caller.
    pub fn evict_fragment(&self, key: &[u8; 32]) -> bool {
        self.generation.load().fragments.evict(key)
    }

    /// The number of cached row-space projection slots currently held (`Building` and `Ready`
    /// both counted) — exposed for tests confirming `Engine::item`'s entity-space visibility test
    /// never constructs one: this must stay `0` across drill-down calls, warm or
    /// cold, unlike `Engine::viewport`'s path, which populates this cache deliberately.
    pub fn row_projection_cache_len(&self) -> usize {
        self.row_projection_cache.len()
    }

    /// How many row projections this engine has built from the whole fragment, rather than derived
    /// from the preceding generation's by unioning the new extents' rows.
    ///
    /// The number to watch after a flush: a deployment where this rises once per session per tick
    /// is paying `Permutation::project` — a *measured* 1 277 ms at 10⁹ — on the steady-state path,
    /// which is the failure write-path §4.6 names. See `RowProjectionCache::get_or_derive`.
    pub fn full_projection_builds(&self) -> u64 {
        self.full_projection_builds.load(Ordering::Relaxed)
    }

    /// Projection builds split by route, in [`crate::compose::ProjectionRoute::ALL`]'s order —
    /// the request path's and the background refresh's together, so it does not sum to
    /// [`Self::full_projection_builds`].
    ///
    /// The distribution is what says whether a deployment's term images are earning anything: a
    /// corpus whose images are written and never read is one whose keep rule or whose chooser
    /// constants do not match the principals it actually serves.
    pub fn projection_builds_by_route(&self) -> [u64; 4] {
        self.projection_routes.counts()
    }

    /// How many walks of the mask and the Morton column this engine has made to resolve a rung of
    /// θ's occupied-tile anchor — see [`crate::occupancy`] and [`crate::stage`].
    ///
    /// **The number to watch is walks per session per publication**, and it should be one per view.
    /// One walk fills every rung at or below the depth it ran at, and the background fill takes it
    /// to `stage::BACKGROUND_DEPTH`, so a session that pans and zooms inside that range should move
    /// this only when the geometry or the overlay does. A deployment where it climbs with request
    /// volume is one where the memo is missing — the key carries the fragment's watermark, so a
    /// flush per request would do it.
    pub fn occupancy_walks(&self) -> u64 {
        self.occupancy_walks.load(Ordering::Relaxed)
    }

    /// Whether the external-id sidecar has opened any extent (or its locator) yet — exposed for
    /// tests confirming `Engine::open` never touches it — the per-extent laziness guarantee.
    pub fn external_id_sidecar_is_open(&self) -> bool {
        self.generation.load().external_index.0.is_open()
    }

    /// Whether the served bundle holds any external-id run: the build's (`--mint-external-ids`)
    /// or one a flush wrote from caller-supplied ids.
    ///
    /// For a route that addresses items by external id and finds that none of the ids it was
    /// given resolve. Against a bundle with no run, that is not a list of ids that name nothing;
    /// it is a deployment nothing can be named in by an external id, and the refusal should say
    /// so once rather than once per member. The check is not inside [`Self::resolve_external_ids`]
    /// because that call also serves `/control/ingest`'s duplicate check, where a deployment with
    /// no external ids yet is the ordinary state of a first batch into an empty database.
    ///
    /// Reads the manifest's run list; opens nothing. Ids ingested with an external id and not yet
    /// flushed live in the write path's live map, which this does not consult: a caller resolves
    /// its ids first, and asks this only when none resolved.
    pub fn bundle_carries_external_ids(&self) -> bool {
        self.generation.load().external_index.0.has_runs()
    }

    /// The write executor's posture — **the liveness signal `readyz` reads**. Ready iff
    /// [`crate::write::ExecutorPosture::Running`].
    ///
    /// Answerable without submitting anything, which is the point: readiness must be a question
    /// about the node, not a side effect of trying to write to it.
    pub fn write_executor_posture(&self) -> crate::write::ExecutorPosture {
        self.write.health().posture()
    }

    /// The executor's counters, for `/control/status`. Operator plane only — bearer-gated, never
    /// on `readyz`, which stays a boolean (SA §9: no internal write-path state on an
    /// unauthenticated surface).
    pub fn write_executor_stats(&self) -> crate::write::ExecutorStats {
        self.write.health().stats()
    }

    /// What the open's shape warm pass did: how many segment pieces were claimed from the
    /// prefix's persisted forms and how many resolved from the geometry (`crate::shapes`).
    /// Operator plane only, beside [`Engine::write_executor_stats`]: counts of structures, naming
    /// no artifact and no principal.
    pub fn shape_warm_report(&self) -> crate::shapes::WarmReport {
        self.shapes.last_warm()
    }

    /// How many artifact row forms, and how many lineages, this engine has built since it opened.
    ///
    /// **The cadence, not the cost.** Both structures are per `(layer, level)` and both are
    /// rebuilt when that level's version moves; what these two numbers answer is how *often* that
    /// happens, which is the question `design/artifact-serving-at-scale.md` §8.1 and §8.2 are
    /// about and the one nothing reported while the store carried a single global version.
    /// Operator plane only, beside [`Engine::write_executor_stats`] — they count structures a
    /// deployment built, and name no artifact, no layer and no principal.
    pub fn artifact_cache_builds(&self) -> (u64, u64) {
        (self.artifact_projections.builds(), self.lineages.builds())
    }

    /// How many artifact row forms, and how many lineages, are held right now.
    ///
    /// The gauge beside [`Engine::artifact_cache_builds`]'s counter, and the one that moves in
    /// both directions: a dropped layer's entries leave both caches at the drop. Operator plane
    /// only — counts of structures, naming no artifact, no layer and no principal.
    pub fn artifact_cache_held(&self) -> (usize, usize) {
        (self.artifact_projections.held(), self.lineages.held())
    }

    /// The supplied-content tables' gauges — see
    /// [`crate::artifact_content::ContentCacheStats`]. Its own accessor rather than a third
    /// element of the two tuples above, because it reports bytes as well as a count and those
    /// two report neither.
    ///
    /// Operator plane only, beside them: counts of structures a deployment built, naming no
    /// artifact, no layer and no principal.
    pub fn artifact_content_cache_stats(&self) -> crate::artifact_content::ContentCacheStats {
        self.level_contents.stats()
    }

    /// How many containment partitions this engine has composed (`crate::containment`).
    ///
    /// **Beside [`Engine::artifact_cache_builds`] because the interesting number is the ratio.**
    /// Under any plugin but the builtin this stays at zero while row forms keep being built, and
    /// containment is on the masked-count route everywhere: a deliberate, fail-closed state rather
    /// than a fault, and an operator has no other way to see it. It also stays below the row-form
    /// count where a bundle carries several views, because the expression is view-independent and
    /// is composed once for all of them. Operator plane only — it names no artifact, no layer and
    /// no principal.
    pub fn artifact_containment_partitions(&self) -> u64 {
        self.artifact_projections.partitions()
    }

    /// How many containment partitions this engine **adopted** from the prefix at open rather than
    /// composing (`crate::containment`).
    ///
    /// The other half of [`Engine::artifact_containment_partitions`]: a deployment that folded and
    /// then restarted should see this at the number of levels it holds and that one at zero. Both
    /// at zero with row forms being built is a foreign plugin; this at zero and that one climbing
    /// is every coordinate rejected — correct, and the expensive answer. Operator plane only.
    pub fn artifact_containment_partitions_adopted(&self) -> u64 {
        self.artifact_projections.adopted()
    }

    /// How many fold-written tile indexes this engine **claimed** from the prefix rather than
    /// deriving (`crate::tile_index`).
    ///
    /// Read beside [`Engine::artifact_cache_builds`]'s first number, which counts the row forms
    /// those indexes belong to: the two equal on a deployment that folded and restarted, and this
    /// one at zero says every coordinate was rejected or every level was published since the fold —
    /// correct, and the expensive answer. Operator plane only; it names no artifact, no layer and
    /// no principal.
    pub fn artifact_tile_indexes_adopted(&self) -> u64 {
        self.artifact_projections.indexes_adopted()
    }

    /// The last compaction fold's per-pass wall clock and resident set — empty before the first
    /// fold. Operator plane only, beside [`Engine::write_executor_stats`].
    pub fn last_fold_passes(&self) -> Vec<crate::compact::PassCost> {
        self.write.health().last_fold_passes()
    }

    /// What the last fold's deletions degraded: the artifacts that lost members, and the supplied
    /// content that lost a source (`annotation-write-cycle.md` §4.2).
    ///
    /// **Operator plane, and the counts here are unmasked** — this is the notice a *publisher* is
    /// owed about their own sets, outside the leak register's viewer scope
    /// ([decision 0024](../../../docs/decisions/0024-operator-credential-is-out-of-scope.md)). No
    /// viewer-facing route may carry these numbers.
    ///
    /// The durable copy is `reports/fold-<prefix>.json` in the bundle root, written before the
    /// fold retires anything and kept when the prefix it reports on is reclaimed. ⊘ No HTTP route
    /// serves this yet.
    pub fn last_fold_report(&self) -> Vec<tessera_lifecycle::membership::Degradation> {
        self.write.health().last_fold_report()
    }

    /// Items in the ingest buffer as of the executor's last apply — what `/control/ingest`'s
    /// occupancy bound is checked against (§1.3).
    ///
    /// **Lags by at most one apply, deliberately.** An exact figure would need the caller to load
    /// the generation, and the bound this feeds is a ceiling with an order of magnitude of
    /// headroom, not a precise quota.
    pub fn buffered_items(&self) -> usize {
        self.write.health().buffered_items.load(Ordering::SeqCst)
    }

    /// Unflushed `POST /control/values` fills the buffer holds (`ingest.md` §1.4), entity-scoped
    /// and group-scoped together.
    ///
    /// **Test- and operator-facing, and what says whether a fill is still pinning the log.** A
    /// fill holds its `ValuesBatch` record against rotation until the flush that writes its cells
    /// consumes it, so a figure that never falls after a restart is the pin that would not
    /// release ([`IngestBuffer::oldest_wal_pos`]).
    pub fn buffered_fills(&self) -> usize {
        self.generation().buffer.fill_count()
    }

    /// One past the lowest row-less entity ever allocated. Operator-facing, beside the point
    /// region's high-water mark on `/control/status`: the two together are how much of the entity
    /// space is left, which neither answers alone.
    pub fn allocator_low_water(&self) -> u64 {
        self.write.live().allocator_low_water()
    }

    /// How many artifacts this node holds, across every layer.
    ///
    /// **Operator-facing, and there is deliberately no per-layer form.** A per-layer count is a
    /// corpus-wide count over objects a principal may not individually see, which is C8's row; the
    /// total answers "is the store populated" for `/control/status` without answering that.
    pub fn published_artifacts(&self) -> usize {
        self.write.live().with_artifacts(|store| store.total())
    }
}
