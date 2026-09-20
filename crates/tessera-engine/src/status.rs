//! The engine's operator gauges and its cache and limit setters.

use std::sync::atomic::{AtomicU64, Ordering};

use tessera_types::TermId;

use crate::engine::Engine;
use crate::Generation;

/// What the serving paths count for an operator, read back through the gauges below.
#[derive(Default)]
pub(crate) struct ServeCounters {
    /// Which route each filtered viewport took to cross its result into row space. Unconditional,
    /// not bench-gated, so it catches a deployment the routing model does not match.
    pub(crate) filter_crossings_projected: AtomicU64,
    pub(crate) filter_crossings_per_tile: AtomicU64,
    /// Filtered viewports whose tree evaluated, wholly or partly, in row space rather than
    /// crossing into it.
    pub(crate) filter_row_routed: AtomicU64,
    /// `member_of` leaves that read the level's row column rather than an artifact-major
    /// membership, whose walk is measurably slower.
    pub(crate) member_of_column_walks: AtomicU64,
    /// Requests served from a one-generation-stale entry.
    pub(crate) stale_serves: AtomicU64,
    /// Entries the background refresh has produced.
    pub(crate) refreshes: AtomicU64,
    /// How many row projections were built from the whole fragment rather than derived from the
    /// preceding generation's. Counted unconditionally, so a test is not gated on a feature flag.
    pub(crate) full_projection_builds: AtomicU64,
    /// Walks of the mask and the Morton column that resolved a rung of `N_occ`'s ladder. A walk
    /// the background fill makes is a walk of the same mask and the same column as a request's,
    /// and counts here too.
    pub(crate) occupancy_walks: AtomicU64,
}

/// One (partition, view)'s live segment count — [`Engine::live_segment_counts`]'s element, and
/// what `/control/status` publishes under `segments`. Defined here, not re-exported from
/// `tessera-store`, since `tessera-server` may not depend on that crate.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ViewSegments {
    pub partition: String,
    pub view: String,
    /// Segments this view's viewport sweep would iterate — base plus every flush extent merge has
    /// not yet collapsed.
    pub segments: usize,
}

/// One partition's live geometry position — [`Engine::partition_status`]'s element, and what
/// `/control/status` publishes as the per-partition block. Defined here for [`ViewSegments`]'
/// reason.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PartitionStatus {
    pub partition: String,
    /// The geometry version: bumped by flush, merge and fold, never by an overlay/buffer update.
    pub segments_version: u64,
    /// The highest entity id folded into published row geometry.
    pub watermark: u64,
}

/// What [`Engine::generation_status`] answers.
#[derive(Debug, Clone)]
pub struct GenerationStatus {
    pub partitions: Vec<PartitionStatus>,
    /// Sorted by `(partition, view)`.
    pub segments: Vec<ViewSegments>,
    pub live_rows: u64,
    pub overlay_depth: usize,
    pub retirable_deletions: u64,
    pub fragment_cache: tessera_authz::fragment::CacheStats,
    pub fragment_cache_rebuilds: u64,
}

fn live_rows_of(generation: &Generation) -> u64 {
    generation
        .bundle
        .partitions
        .values()
        .flat_map(|partition| partition.manifest.segments.iter())
        .map(|descriptor| u64::from(descriptor.row_count))
        .sum()
}

/// Live segments per (partition, view). A viewport pays a measured 1.4-1.6 µs per
/// (tile × segment). Sorted, since the generation holds them in `HashMap`s and an operator diffing
/// two responses must not see a reordering that means nothing.
fn segment_counts_of(generation: &Generation) -> Vec<ViewSegments> {
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

/// Each partition's live `(segments_version, watermark)`, sorted by partition.
fn partition_status_of(generation: &Generation) -> Vec<PartitionStatus> {
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
    rows.sort_by(|a, b| a.partition.cmp(&b.partition));
    rows
}

impl Engine {
    /// Requests served from a one-generation-stale entry, and entries the background refresh has
    /// produced. Read together: the second rising while [`Self::full_projection_builds`] does not
    /// means the refresh is keeping up.
    pub fn stale_serves(&self) -> u64 {
        self.counters.stale_serves.load(Ordering::Relaxed)
    }

    /// See [`Self::stale_serves`].
    pub fn refreshes(&self) -> u64 {
        self.counters.refreshes.load(Ordering::Relaxed)
    }

    /// Filtered viewports served by each crossing route, `(projected, per_tile)`. Unfiltered
    /// requests are counted in neither.
    pub fn filter_crossing_routes(&self) -> (u64, u64) {
        (
            self.counters.filter_crossings_projected.load(Ordering::Relaxed),
            self.counters.filter_crossings_per_tile.load(Ordering::Relaxed),
        )
    }

    /// Filtered viewports that evaluated in row space. A mixed tree counts here and in whichever
    /// crossing its entity sub-trees took.
    pub fn filter_row_routes(&self) -> u64 {
        self.counters.filter_row_routed.load(Ordering::Relaxed)
    }

    /// `member_of` leaves served by the row-column walk rather than by the artifact-major
    /// membership.
    pub fn member_of_column_walks(&self) -> u64 {
        self.counters.member_of_column_walks.load(Ordering::Relaxed)
    }

    /// Delegates to `WritePath::allocator_high_water`, which owns the allocator.
    pub fn allocator_high_water(&self) -> u64 {
        self.write.live().allocator_high_water()
    }

    /// The masked-count cache's gauges. Operator plane only: a count of structures, naming no
    /// artifact and no principal.
    pub fn masked_count_cache_stats(&self) -> crate::histogram::MaskedCountStats {
        self.masked_counts.stats()
    }

    /// The derived-geometry cache's gauges. Operator plane only, naming no artifact and no
    /// principal. `hit_rate` is the figure it is judged on: the same principal panning across one
    /// layer re-serves mostly the same artifacts.
    pub fn derived_cache_stats(&self) -> crate::derived_cache::DerivedCacheStats {
        self.derived_geometry.stats()
    }

    /// Bound the derived-geometry cache. Unset, an embedder gets `crate::derived_cache`'s default;
    /// there is no configuration key, since an entry's size is bounded by the vertex budget rather
    /// than the corpus.
    pub fn set_derived_cache_bytes(&self, bytes: u64) {
        self.derived_geometry.set_bound_bytes(bytes);
    }

    /// How many levels are recorded row-major and served artifact-major.
    pub fn layout_fallbacks(&self) -> u64 {
        self.artifact_projections.layout_fallbacks()
    }

    /// How many fold-written row-major columns were claimed rather than composed.
    pub fn columns_adopted(&self) -> u64 {
        self.artifact_projections.columns_adopted()
    }

    /// How many row-major columns were composed from a level's row form rather than claimed.
    pub fn columns_composed(&self) -> u64 {
        self.artifact_projections.columns_composed()
    }

    /// The serving layout recorded for one `(layer, level)`, or `None` where no such layer is
    /// registered. Operator plane only: nothing on the wire carries it.
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

    /// Bound both caches; the only route by which the two config keys reach them. Called by
    /// `tessera_server::prepare` after it validates both figures.
    pub fn set_cache_bounds(&self, row_projection_bytes: u64, fragment_bytes: u64) {
        self.row_projection_cache
            .set_bound_bytes(row_projection_bytes);
        self.generation
            .load()
            .fragments
            .set_memory_bound(fragment_bytes);
    }

    /// Bound the masked-count cache (`serve.masked_count_cache_bytes`). This key exists only for a
    /// deployment with a row-major layer; unset, an embedder gets an unbounded cache.
    pub fn set_masked_count_cache_bytes(&self, bytes: u64) {
        self.masked_counts.set_bound_bytes(bytes);
    }

    /// Bound the region decomposition cache (`serve.region_cache_bytes`).
    pub fn set_region_cache_bytes(&self, bytes: u64) {
        self.region_cache.set_bound_bytes(bytes);
    }

    /// `serve.max_region_cells`: the boundary-cell budget a region's descent stops at, published
    /// on `/v1/meta`. Default [`crate::region::DEFAULT_MAX_REGION_CELLS`].
    pub fn set_max_region_cells(&self, cells: usize) {
        self.max_region_cells.store(cells as u64, Ordering::Relaxed);
    }

    /// The region cache's gauges, beside the row-projection cache's.
    pub fn region_cache_stats(&self) -> tessera_cache::CacheStats {
        self.region_cache.stats()
    }

    /// The occupancy memo's gauges, one entry per `(session, view, depth, generation)` rung.
    /// `evictions` rising is the memo removing rungs taken against a superseded generation.
    pub fn occupancy_cache_stats(&self) -> tessera_cache::CacheStats {
        self.occupancy.stats()
    }

    /// Bound the occupancy memo. Unset, an embedder gets [`crate::occupancy::DEFAULT_OCCUPANCY_CACHE_BYTES`].
    pub fn set_occupancy_cache_bytes(&self, bytes: u64) {
        self.occupancy.set_bound_bytes(bytes);
    }

    /// How long a request parks on another request's in-flight row-projection build before it is
    /// refused (`serve.single_flight_wait_ms`). Unset, an embedder gets [`crate::DEFAULT_SINGLE_FLIGHT_WAIT_MS`].
    pub fn set_single_flight_wait_ms(&self, wait_budget_ms: u64) {
        self.row_projection_cache.set_wait_budget_ms(wait_budget_ms);
    }

    /// The row count at which a commit window closes (`ingest.commit_window_max_items`, which
    /// counts rows). `usize::MAX` here is unbounded, not "off". `0` is clamped to `1` rather than
    /// treated as "close at zero rows"; `tessera-server`'s config refuses it outright.
    pub fn set_commit_window_max_rows(&self, rows: usize) {
        self.write.health().set_commit_window_max_rows(rows);
    }

    /// The overlay depth at which the executor raises an alarm.
    ///
    /// Checked here as well as on every later deny apply: a WAL replay builds an overlay before
    /// any executor exists, so a node restarting above the limit would otherwise start with the
    /// alarm counter at zero. `usize::MAX` disables the alarm; `tessera-server`'s config refuses
    /// `0`.
    pub fn set_overlay_soft_limit(&self, limit: usize) {
        self.write.health().set_overlay_soft_limit(limit);
        let depth = self.overlay_depth();
        // Re-arms the edge trigger, so a limit landing under a live overlay alarms once here.
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

    /// Everything `/control/status` reads off the live generation, from one load, so the figures
    /// in one response describe one publication.
    pub fn generation_status(&self) -> GenerationStatus {
        let generation = self.generation.load();
        GenerationStatus {
            partitions: partition_status_of(&generation),
            segments: segment_counts_of(&generation),
            live_rows: live_rows_of(&generation),
            overlay_depth: generation.overlay.len(),
            retirable_deletions: generation.overlay.deleted_len(),
            fragment_cache: generation.fragments.stats(),
            fragment_cache_rebuilds: generation.fragments.rebuild_count(),
        }
    }

    /// The live overlay's entry count, read off the current generation.
    pub fn overlay_depth(&self) -> usize {
        self.generation.load().overlay.len()
    }

    /// Rows the bundle's segments hold, tombstoned ones included: the only figure on
    /// `/control/status` giving the corpus's actual size.
    pub fn live_rows(&self) -> u64 {
        live_rows_of(&self.generation.load())
    }

    /// Retirable deletions: `|deleted|`, never the union with `suppressed`. Read beside
    /// [`Engine::overlay_depth`]: this is what a fold can reduce, since a suppression never retires.
    pub fn retirable_deletions(&self) -> u64 {
        self.generation.load().overlay.deleted_len()
    }

    /// The row-projection cache's operator gauges, published as `projection_cache`. The fragment
    /// tier's twin is [`Self::fragment_cache_stats`].
    pub fn row_projection_cache_stats(&self) -> tessera_cache::CacheStats {
        self.row_projection_cache.stats()
    }

    /// The fragment cache's operator gauges. `tessera_engine::FragmentCacheStats` is the name a
    /// caller outside this crate should use, since `tessera-server` may not depend on
    /// `tessera-authz`. This and the three methods below expose only `stats`, `evict`,
    /// `canonical_key_for` and `rebuild_count`, not the whole cache: `FragmentCache::set_memory_bound`
    /// would silently undo the bound [`Self::set_cache_bounds`]'s validated caller sets.
    pub fn fragment_cache_stats(&self) -> tessera_authz::fragment::CacheStats {
        self.generation.load().fragments.stats()
    }

    /// Times the fragment cache has actually re-unioned postings, rather than reopening a
    /// digest-verified `.frag` sidecar or hitting the in-memory tier. Read off the live
    /// generation's cache, so this and [`Self::fragment_cache_stats`] both reset at a compaction.
    pub fn fragment_cache_rebuilds(&self) -> u64 {
        self.generation.load().fragments.rebuild_count()
    }

    /// The canonical cache key for `satisfied`, at the live generation's watermark — the only way
    /// to name a fragment entry from outside, and what [`Self::evict_fragment`] takes. Pure;
    /// reveals nothing the caller did not supply.
    pub fn fragment_canonical_key(&self, satisfied: &[TermId]) -> [u8; 32] {
        let generation = self.generation.load();
        generation
            .fragments
            .canonical_key_for(satisfied, generation.watermark)
    }

    /// Drop one entry from the fragment cache's in-memory tier; returns whether it was there. The
    /// digest-verified `.frag`/`.meta` pair stays on disk, and a live `Session` holding the fragment
    /// keeps its mapping alive regardless.
    pub fn evict_fragment(&self, key: &[u8; 32]) -> bool {
        self.generation.load().fragments.evict(key)
    }

    /// Cached row-space projection slots currently held (`Building` and `Ready` both counted).
    /// Stays `0` across drill-down calls, unlike `Engine::viewport`'s path, which populates it.
    pub fn row_projection_cache_len(&self) -> usize {
        self.row_projection_cache.len()
    }

    /// Row projections built from the whole fragment, rather than derived from the preceding
    /// generation's by unioning the new extents' rows. Rising once per session per tick after a
    /// flush means a full build, measured at 1 277 ms at 10⁹, is on the steady-state path.
    pub fn full_projection_builds(&self) -> u64 {
        self.counters.full_projection_builds.load(Ordering::Relaxed)
    }

    /// Projection builds split by route, in [`crate::projection::ProjectionRoute::ALL`]'s order; does
    /// not sum to [`Self::full_projection_builds`] since it excludes the background refresh.
    pub fn projection_builds_by_route(&self) -> [u64; 4] {
        self.projection_routes.counts()
    }

    /// Walks of the mask and the Morton column made to resolve an occupied-tile anchor — see
    /// [`crate::occupancy`] and [`crate::stage`]. Should run about once per session per
    /// publication per view; climbing with request volume means the memo is missing.
    pub fn occupancy_walks(&self) -> u64 {
        self.counters.occupancy_walks.load(Ordering::Relaxed)
    }

    /// Whether the external-id sidecar has opened any extent yet — exposed for tests confirming
    /// `Engine::open` never touches it.
    pub fn external_id_sidecar_is_open(&self) -> bool {
        self.generation.load().external_index.0.is_open()
    }

    /// Whether the served bundle holds any external-id run, from the build or from a flush. Lets a
    /// route resolving external ids say once that a bundle carries none, rather than once per id.
    /// Reads the manifest's run list; opens nothing, and does not consult ids not yet flushed.
    pub fn bundle_carries_external_ids(&self) -> bool {
        self.generation.load().external_index.0.has_runs()
    }

    /// The write executor's posture: the liveness signal `readyz` reads, answerable without
    /// submitting anything.
    pub fn write_executor_posture(&self) -> crate::write::ExecutorPosture {
        self.write.health().posture()
    }

    /// The executor's counters. Bearer-gated, never on `readyz`, which stays a boolean: no internal
    /// write-path state belongs on an unauthenticated surface.
    pub fn write_executor_stats(&self) -> crate::write::ExecutorStats {
        self.write.health().stats()
    }

    /// What the open's shape warm pass did: segment pieces claimed from the prefix's persisted
    /// forms versus resolved from the geometry. Operator plane only.
    pub fn shape_warm_report(&self) -> crate::shapes::WarmReport {
        self.shapes.last_warm()
    }

    /// Artifact row forms, and lineages, built since this engine opened: cadence, not cost, since
    /// both rebuild whenever a `(layer, level)`'s version moves.
    pub fn artifact_cache_builds(&self) -> (u64, u64) {
        (self.artifact_projections.builds(), self.lineages.builds())
    }

    /// Artifact row forms, and lineages, held right now. Moves in both directions, unlike
    /// [`Engine::artifact_cache_builds`]: a dropped layer's entries leave both caches at the drop.
    pub fn artifact_cache_held(&self) -> (usize, usize) {
        (self.artifact_projections.held(), self.lineages.held())
    }

    /// The supplied-content tables' gauges. Its own accessor, not a third element of the two tuples
    /// above, since it reports bytes as well as a count.
    pub fn artifact_content_cache_stats(&self) -> crate::artifact_content::ContentCacheStats {
        self.level_contents.stats()
    }

    /// Containment partitions this engine has composed. Stays at zero under any plugin but the
    /// builtin, since containment then runs on the masked-count route instead.
    pub fn artifact_containment_partitions(&self) -> u64 {
        self.artifact_projections.partitions()
    }

    /// Containment partitions adopted from the prefix at open rather than composed. A deployment
    /// that folded and restarted should see this near the level count and the composed figure near
    /// zero.
    pub fn artifact_containment_partitions_adopted(&self) -> u64 {
        self.artifact_projections.adopted()
    }

    /// Fold-written tile indexes claimed from the prefix rather than derived. Equal to
    /// [`Engine::artifact_cache_builds`]'s first number on a deployment that folded and restarted.
    pub fn artifact_tile_indexes_adopted(&self) -> u64 {
        self.artifact_projections.indexes_adopted()
    }

    /// The last compaction fold's per-pass wall clock and resident set; empty before the first
    /// fold.
    pub fn last_fold_passes(&self) -> Vec<crate::compact::PassCost> {
        self.write.health().last_fold_passes()
    }

    /// What the last fold's deletions degraded: artifacts that lost members, supplied content that
    /// lost a source. Unmasked, on the operator credential only: no viewer-facing route may carry
    /// these numbers. The durable copy is `reports/fold-<prefix>.json` in the bundle root; no HTTP
    /// route serves this yet.
    pub fn last_fold_report(&self) -> Vec<tessera_lifecycle::membership::Degradation> {
        self.write.health().last_fold_report()
    }

    /// Items in the ingest buffer as of the executor's last apply — what `/control/ingest`'s
    /// occupancy bound is checked against. Lags by at most one apply.
    pub fn buffered_items(&self) -> usize {
        self.write.health().buffered_items.load(Ordering::SeqCst)
    }

    /// Unflushed `POST /control/values` fills the buffer holds, entity- and group-scoped together.
    /// A figure that never falls after a restart is a fill pinning the log that will not release.
    pub fn buffered_fills(&self) -> usize {
        self.generation().buffer.fill_count()
    }

    /// One past the lowest row-less entity ever allocated. Read beside the high-water mark:
    /// together they say how much of the entity space is left.
    pub fn allocator_low_water(&self) -> u64 {
        self.write.live().allocator_low_water()
    }

    /// How many artifacts this node holds, across every layer. No per-layer form: that would be a
    /// corpus-wide count over objects a principal may not individually see.
    pub fn published_artifacts(&self) -> usize {
        self.write.live().with_artifacts(|store| store.total())
    }
}
