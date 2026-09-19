//! The engine's state, the bundle open protocol and the shared compute pool.

use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicU64};
use std::sync::Arc;

use arc_swap::ArcSwap;
use rand::rngs::OsRng;
use rand::RngCore;
use rustc_hash::FxHashMap;
use tessera_authz::{DeltaTier, Dict, FragmentCache, PostingsReader};
use tessera_lifecycle::wal::ChangeOp;
use tessera_plugin::Plugin;
use tessera_store::{Bundle, StoreError};
use tessera_store::manifest::CurrentPointer;
use tessera_store::read::open_bundle;
use tessera_store::vocabulary::Vocabularies;
use tessera_types::{EntityId, IdentityKey};

use crate::{Generation, GenerationHandle};
use crate::cache::RowProjectionCache;
use crate::config::{coalesce_policy, merge_policy, EngineConfig};
use crate::error::{EngineError, Result};
use crate::geometry::GeometryPublication;
use crate::write::{PublishGeometryError, WritePath};

/// Build the shared compute pool, **with the panic handler every pooled task depends on**.
///
/// One constructor rather than a `ThreadPoolBuilder` at each site, because the handler is not a
/// refinement of the pool: it is the difference between a diagnosable failure and an unattributable
/// one, and a second pool built without it would silently be the old behaviour.
///
/// # What rayon does with a panic, and why the two APIs differ
///
/// `install`, `join` and `scope` have an obvious caller to propagate a panic to, and they do —
/// `Engine::viewport`'s tile sweep relies on exactly that, and the panic handler is **not** invoked
/// for them ([`tests::a_panic_inside_the_shared_pool_propagates_to_the_caller`] pins it, and would
/// abort this process instead of passing if that changed). `spawn` has no such caller: the write
/// path's flush, merge and coalesce submit their result through a channel and nobody is waiting on
/// the closure. With no handler configured, rayon's answer to a panic there is to **abort the
/// process** — one line, no payload, no backtrace, and under `libtest` the panic's own message is
/// discarded with the captured output of a test the runner never gets to name. That is what made a
/// `debug_assert` anywhere inside flush, merge or coalesce undiagnosable in a debug build.
///
/// # The record, and why it is written twice
///
/// [`describe_pool_panic`] names the subsystem, the worker thread and the payload, and carries a
/// backtrace of the **abort site** — the handler's own stack, since rayon calls it after unwinding
/// has finished. The panic's own location is on the line the default panic hook already printed;
/// what this adds is the attribution, and a copy that survives.
///
/// It goes to `tracing` for a deployment, which has a subscriber, and directly to `stderr` for
/// everything that does not — a test binary above all, where `libtest`'s capture is discarded with
/// the process the next line aborts.
///
/// **Aborting is deliberately unchanged.** A pooled task that panicked left its `in_flight` flag
/// set — the store that clears it is the last statement of the closure the unwind skipped — so
/// continuing would wedge the flush, merge or coalesce it was, silently and for the process's
/// lifetime. Whether that should instead reach the write path's `Dead` posture, which refuses
/// callers and says why, is a design question this does not settle.
pub(crate) fn build_compute_pool(
    threads: usize,
) -> std::result::Result<rayon::ThreadPool, rayon::ThreadPoolBuildError> {
    rayon::ThreadPoolBuilder::new()
        .num_threads(threads)
        .panic_handler(|payload| {
            let record = describe_pool_panic(payload.as_ref());
            tracing::error!(pool_panic = %record, "a task spawned on the shared compute pool panicked");
            // Directly, not through `eprintln!`: `libtest` captures the print macros and drops
            // what it captured when the process below dies, which is the whole reason this record
            // exists.
            let _ = std::io::Write::write_all(
                &mut std::io::stderr().lock(),
                format!("{record}\n").as_bytes(),
            );
            std::process::abort();
        })
        .build()
}

/// The record a pooled task's panic leaves: subsystem, worker thread, payload, abort-site
/// backtrace.
///
/// Separate from the handler so it can be asserted without aborting the process that asserts it.
fn describe_pool_panic(payload: &(dyn std::any::Any + Send)) -> String {
    let message = payload
        .downcast_ref::<&'static str>()
        .map(|s| (*s).to_string())
        .or_else(|| payload.downcast_ref::<String>().cloned())
        .unwrap_or_else(|| "a payload that is neither &str nor String".to_string());
    let thread = std::thread::current()
        .name()
        .unwrap_or("<unnamed>")
        .to_string();
    format!(
        "tessera: a task spawned on the shared compute pool panicked; aborting\n  \
         worker thread: {thread}\n  payload: {message}\n  abort-site backtrace (the panic's own \
         location is on the default hook's line above):\n{}",
        std::backtrace::Backtrace::force_capture()
    )
}

/// The request-serving engine: one immutable [`Generation`] behind an atomically-swappable
/// pointer, plus the state that genuinely is process-lifetime — the plugin, the compute pool, the
/// `tessera_id` key, the row-projection cache and the bundle root.
///
/// **The dictionary, the postings reader, the fragment cache and the external-id sidecar are not
/// among them, and used to be.** Each is per-generation because something publishes a new one: a
/// flush promotes into the dictionary, a fold rewrites the term index and rotates the fragment
/// identity, and a fold or a coalesce rewrites the external-id runs. Holding them here made a
/// publication that changed any of them inexpressible — see [`Generation::fragments`] for what
/// that cost.
pub struct Engine {
    /// The live generation pointer. `Arc`-shared with [`WritePath`], which publishes every
    /// generation swap through this exact pointer: the write path owns the swap, the
    /// read paths own the load, and both must see one pointer or a swap would be invisible.
    pub(crate) generation: Arc<GenerationHandle>,
    pub(crate) plugin: Arc<dyn Plugin>,
    /// The row-projection cache — see [`RowProjectionCache`]'s own doc.
    pub(crate) row_projection_cache: Arc<RowProjectionCache>,
    /// A region leaf's decomposition per `(view, generation, canonical shape, stop depth)` —
    /// see [`crate::region`]. Keyed on **no principal**, deliberately: the entry carries no
    /// authorisation, and the rows inside the shape are tested under each request's own mask
    /// rather than held (owner ruling 2026-08-29, selection-operand §10 (b)).
    pub(crate) region_cache: Arc<
        crate::single_flight::SingleFlightCache<
            crate::region::RegionKey,
            crate::region::RegionDecomposition,
        >,
    >,
    /// `serve.max_region_cells` — the most boundary cells a region's descent may hold at one
    /// depth before it answers a cover (selection-operand §6). A setter rather than an
    /// `EngineConfig` field, for [`Engine::set_masked_count_cache_bytes`]'s reason.
    pub(crate) max_region_cells: AtomicU64,
    /// Artifact memberships in row space, one entry per `(view, layer, level)` — see
    /// [`ArtifactProjections`]. Distinct from the cache above and deliberately so: that one is
    /// keyed per *session* (a principal's own visible set), this one per *deployment* (what a layer
    /// published), and they move on different events.
    pub(crate) artifact_projections: Arc<crate::artifacts::ArtifactProjections>,
    /// The spatial levels' held shapes, index and per-segment resolved pieces
    /// (`crate::shapes`) — built at open and at every publication into a shape layer, filled by
    /// every flush before its publication, and joined per generation into the row form above.
    pub(crate) shapes: Arc<crate::shapes::ShapeStore>,
    /// The masked-count histograms of the levels served **row-major**, per `(session, layer,
    /// level)` — [decision 0093](../../../docs/decisions/0093-nothing-is-materialised-per-token-over-the-artifact-population.md)'s
    /// one named exception, byte-budgeted exactly as the row-projection cache is.
    ///
    /// **Beside the projections rather than inside them, because the cadences differ**: a row form
    /// is per deployment and moves when a level is published, and this is per *session* and moves
    /// when the principal's mask does — which includes every accepted deny. Empty for a deployment
    /// with no row-major level, which is most of them.
    pub(crate) masked_counts: Arc<crate::histogram::MaskedCountCache>,
    /// `N_occ(d)` per `(session, view, depth)` and generation — §7.2's second θ anchor, memoised
    /// so a pan at one zoom does not walk the mask again. See [`crate::occupancy`] for the walk
    /// and [`crate::occupancy::OccupancyKey`] for why each term is in the key.
    ///
    /// **Per *session* like the two caches below it**, and for the same reason: `N_occ` is counted
    /// inside one principal's own composed mask, so an entry is never shared across principals.
    /// Superseded generations age out under the byte bound rather than being pruned at a swap: an
    /// entry is a `u64` under a key naming its generation, so a stale one is unreachable rather
    /// than wrong.
    pub(crate) occupancy: Arc<
        crate::single_flight::SingleFlightCache<
            crate::occupancy::OccupancyKey,
            crate::occupancy::OccupiedTiles,
        >,
    >,
    /// One artifact's derived centroid, box and hull, per principal — see
    /// [`crate::derived_cache::DerivedCache`]. Per *session* like the histograms beside it and for
    /// the same reason: the values are functions of the principal's own visible members, so an
    /// entry is never shared across principals.
    pub(crate) derived_geometry: Arc<crate::derived_cache::DerivedCache>,
    /// One session's visible values per category column — see [`crate::suggest_set::SuggestSets`].
    /// Per *session* like the two caches above it, and keyed on the generation and the overlay for
    /// the reason stated there: a set taken before a suppression would keep offering the name of a
    /// value whose last visible member has gone.
    pub(crate) suggest_sets: Arc<crate::suggest_set::SuggestSets>,
    /// One lineage per `(layer, level)` — see [`crate::cut::Lineages`]. Keyed per *deployment* like
    /// the projections beside it, and on the store's version alone, because a level's parent
    /// pointers are the same whichever view is served.
    pub(crate) lineages: Arc<crate::cut::Lineages>,
    /// One supplied-content table per `(layer, level)` — see
    /// [`crate::artifact_content::LevelContents`]. Keyed per *deployment* like the two caches
    /// above it: what an artifact's name says is a property of what was published, and the
    /// verdict that decides whether a viewer is served it runs before this is read.
    pub(crate) level_contents: Arc<crate::artifact_content::LevelContents>,
    /// D-D: the ONE shared compute pool every admitted `viewport` request's tile loop `install`s
    /// onto (`Engine::viewport`). Built once, here, at open — never per request, and never a
    /// second pool anywhere else in this crate (no nested throttling). `pool.install` from more
    /// external (server-side) threads than this pool has workers only queues on rayon's injector;
    /// it does not deadlock.
    /// D-D's one shared compute pool — and, since flush, the pool a segment write executes on
    /// (§1.1). `Arc` because the executor thread holds it too; still exactly one pool.
    pub(crate) pool: Arc<rayon::ThreadPool>,
    /// The bundle **root** — the directory holding `CURRENT` and every prefix under it.
    ///
    /// **The root, not the prefix directory, and that is the fourth gap of compaction §4.** A
    /// prefix directory captured once is correct for as long as nothing can publish a new prefix,
    /// which is exactly the premise a fold breaks: the first deny published after a flip would
    /// write its side-manifest into the prefix reclamation is about to delete — acked deny state,
    /// gone from the restore path, with no error anywhere. A second copy that rotates is not the
    /// fix either; it is one more thing to miss at one of eight call sites. The prefix directory
    /// is **derived** from the live generation's own `prefix` wherever it is needed
    /// (`Executor::prefix_dir`), so it cannot go stale by construction.
    pub(crate) bundle_root: std::path::PathBuf,
    /// Where this engine writes its suggestion indexes — `<cache dir>/suggest`, engine-local and
    /// never in the bundle (`crate::suggest`'s header).
    pub(crate) suggest_dir: std::path::PathBuf,
    pub(crate) config: EngineConfig,
    pub(crate) next_token_id: AtomicU64,
    /// The write path: the WAL, the I9 allocator, the live external-id maps, the resolver's
    /// extension state and the idempotency index. Every mutating engine method below is
    /// a thin delegation to this; the read paths that need write-side state (`resolve_external_id`
    /// and its two siblings) compose over its read accessors, so this crate has one owner for each
    /// mutable field rather than two.
    pub(crate) write: WritePath,
    /// The `tessera_id` blinding permutation's per-deployment key (contracts §2.6 r6, design
    /// memo `docs/evidence/memos/2026-07-30-tessera-id-construction.md`) — parsed once at open from
    /// MANIFEST's `identity.key` and held for the process lifetime. Never leaves the server (I10).
    /// `IdentityKey`'s `Debug` is redacted and it has no hex accessor, so *this* field cannot be
    /// logged; the plaintext hex carried beside it in MANIFEST is redacted at its own carriers
    /// (`IdentityDescriptor`'s and `BuildArgs`' hand-written `Debug` impls print a fingerprint) —
    /// stated precisely because "the key is never logged" is a property of every carrier, not of
    /// this type alone. `pub(crate)`: `viewport.rs`'s
    /// `Engine::item` inverts a caller-supplied `tessera_id` with it directly.
    pub(crate) identity_key: IdentityKey,
    /// A per-process random value folded into every content key (`delta-serving.md` §2).
    ///
    /// **Without it a content key can collide across a restart.** `overlay_version` is an
    /// in-process counter that starts at zero, so a post-restart key would repeat a pre-restart
    /// one over different overlay content, and a client's held declaration would be honoured
    /// against a visible set it was never computed for. Costs only that declarations lapse when
    /// the process restarts, which the render/declare split makes invisible to a user.
    ///
    /// Not a secret and not a key: it never has to be unpredictable, only distinct.
    pub(crate) boot_nonce: u64,
    /// The effective serial/parallel fan-out threshold
    /// (`viewport::SERIAL_FALLBACK_MAX_ROWS`) this engine reads on every `viewport` call,
    /// defaulted at `open` to that constant and never otherwise written in production. Exists so
    /// `set_serial_fallback_max_rows_for_test` (below) has something per-`Engine` to override —
    /// see that method's doc for why this lives here rather than as global or thread-local state.
    /// `pub(crate)`: `viewport.rs`'s `Engine::viewport` (a different module, same crate) reads it
    /// on every request.
    pub(crate) serial_fallback_max_rows: AtomicU64,
    /// Which route each filtered viewport took to cross its result into row space — projected, and
    /// tested per tile (`viewport::Engine::cross_filter_into_row_space`).
    ///
    /// **Unconditional, not `bench-timing`-gated**, for the same reason
    /// [`Self::full_projection_builds`] is: the constant that chooses between the two routes
    /// (`viewport::PER_TILE_CROSSING_RATIO`) is calibrated from a single-threaded probe at one
    /// scale, and the way a calibration like that is found to be wrong is a deployment where the
    /// split is nothing like what the model predicts. A number only a bench build can see is not
    /// that observable.
    pub(crate) filter_crossings_projected: AtomicU64,
    pub(crate) filter_crossings_per_tile: AtomicU64,
    /// Filtered viewports whose tree evaluated (wholly or partly) in **row space** — decision
    /// 0068's route, taken when a leaf's column affords only that route or when
    /// `rows_in_ranges ≤ |M_auth|` prefers it. Unconditional for the reason the two crossing
    /// counters are: the route rule is the calibrated claim, and this is the observable that
    /// catches it being wrong in a deployment no bench reproduces. The two crossing counters
    /// still count the one crossing such a request makes for its entity-space sub-trees; a
    /// pure-row tree crosses nothing and moves this counter alone.
    pub(crate) filter_row_routed: AtomicU64,
    /// `member_of` leaves that read the level's row column rather than an artifact-major
    /// membership (`viewport::Engine::resolve_member_of`).
    ///
    /// **It rises on every level served column-only** — one whose row column and whose extents the
    /// prefix both hold, which builds no artifact-major form
    /// (`crate::artifacts::MembershipRows::rows_held`). The walk is bounded by the artifact's own
    /// extent and by the visible set, where the bitmap answers in one intersection: the unbounded
    /// form of it was measured at 2.85 s against 22 ms on rung 3's `mesh/descriptors`
    /// (2026-09-02). This is the number that says which levels are paying that difference.
    pub(crate) member_of_column_walks: AtomicU64,
    /// Requests served from a one-generation-stale entry — the steady-state observable behind
    /// decision 0044's stale-serve. A deployment where this rises and
    /// [`Self::full_projection_builds`] does not is one where the refresh is keeping up.
    pub(crate) stale_serves: AtomicU64,
    /// Entries the background refresh has produced — the observable behind "the refresh is
    /// keeping up", read beside [`Self::full_projection_builds`].
    pub(crate) refreshes: Arc<AtomicU64>,
    /// Whether a background refresh is producing the live generation's entries.
    ///
    /// **Set before the swap and cleared when the pass ends**, which is what makes the 429 rung of
    /// `Engine::session_geometry`'s ladder bounded rather than open-ended: a racer landing between
    /// the swap and the pool task's first insert must see `true`, or after a merge it takes the
    /// measured 1.28 s rebuild inline (review finding F5). Shared with the executor, which is the
    /// only writer.
    pub(crate) refresh_in_flight: Arc<AtomicU64>,
    /// Whether the background refresh runs — see [`crate::refresh::RefreshDeps::enabled`].
    pub(crate) refresh_enabled: Arc<AtomicBool>,
    /// Whether the background refresh **holds** — see [`crate::refresh::RefreshDeps::paused`].
    pub(crate) refresh_paused: Arc<AtomicBool>,
    /// Whether the entity-space coalesce runs. Always `true` in a shipped build.
    pub(crate) coalesce_enabled: Arc<AtomicBool>,
    /// Whether the row-space merge runs. Always `true` in a shipped build; a test turns it off to
    /// hold the entity-space axes still, since a merge coalesces runs and locator extents of its
    /// own and the two passes would otherwise race for the same entries.
    pub(crate) merge_enabled: Arc<AtomicBool>,
    /// Whether a fold **holds** between finishing its passes and submitting the result — see
    /// [`Engine::set_fold_paused_for_test`]. Always `false` in a shipped build.
    pub(crate) fold_paused: Arc<AtomicBool>,
    /// See [`Engine::set_fold_publication_paused_for_test`]. Always `false` in a shipped build.
    pub(crate) fold_publication_paused: Arc<AtomicBool>,
    /// See [`Engine::set_merge_publication_paused_for_test`]. Always `false` in a shipped build.
    pub(crate) merge_publication_paused: Arc<AtomicBool>,
    /// How many row projections were built from the whole fragment rather than derived from the
    /// preceding generation's — the observable behind [`Engine::full_projection_builds`].
    ///
    /// **Unconditional, not `bench-timing`-gated**, unlike `StageTimings::row_projection_built`.
    /// The property it makes testable — that a flush does not cost every session a full rebuild —
    /// is a correctness-shaped one for a deployment's latency, and a test that only runs under a
    /// feature flag is a test that does not run.
    pub(crate) full_projection_builds: AtomicU64,
    /// Every full projection build split by the route it took, and the route a test has fixed —
    /// see [`crate::compose::ProjectionRoutes`].
    ///
    /// **Shared with the background refresh**, which is the other place a full projection is
    /// built, and counted from both. It therefore does not sum to
    /// [`Self::full_projection_builds`], which counts the request path alone and keeps that
    /// meaning.
    ///
    /// **Unconditional, not `bench-timing`-gated**, on [`Self::full_projection_builds`]' argument.
    /// The chooser's constants are modelled from one probe at one scale, and the way a model like
    /// that is found to be wrong in a deployment is a route distribution nothing predicted: every
    /// session walking where the images were written to be read, or every session reading images
    /// for a residual that swamps them. `full_projection_builds` alone cannot see either.
    pub(crate) projection_routes: Arc<crate::compose::ProjectionRoutes>,
    /// Walks of the mask and the Morton column that resolved a rung of `N_occ`'s ladder — the
    /// observable behind [`Engine::occupancy_walks`].
    ///
    /// **Unconditional, not `bench-timing`-gated**, on [`Self::full_projection_builds`]' argument:
    /// the claim this change makes is that a session pays one walk per `(view, generation)` and not
    /// one per depth it visits, and the way that claim is found to be wrong in a deployment is a
    /// counter that climbs with requests rather than with publications.
    pub(crate) occupancy_walks: Arc<AtomicU64>,
    /// **The occupancy stage** — see [`crate::stage`]. θ's `N_occ(d)` anchor is memoised per depth
    /// and was therefore paid on the first request at each new depth; this is what fills the rest
    /// of the ladder on the pool once one request has, cancellably.
    pub(crate) stage: crate::stage::StageDeps,
}

/// Every deny disposition the bundle's side-manifests carry, as overlay operations.
///
/// `deny` is the current *suppression* set and `tombstones` names entities already deleted — the
/// two deny-disposition fields of contracts §2.3, and the two `HONOURED_STATE` claims this
/// function discharges. Read across every partition, because the overlay is engine-wide while a
/// side-manifest is per-partition.
///
/// **`Suppress` and `Delete`, never `Unsuppress`.** A manifest carries the suppression set as it
/// currently stands, so an unsuppressed entity is simply absent from it; inventing an
/// `Unsuppress` for an absent entity would let an older manifest clear a suppression the WAL
/// still holds.
fn initial_deny_of(bundle: &Bundle) -> Vec<(EntityId, ChangeOp)> {
    let mut out = Vec::new();
    for partition in bundle.partitions.values() {
        for entry in &partition.manifest.deny {
            out.push((EntityId::new(entry.entity_id), ChangeOp::Suppress));
        }
        for &entity_id in &partition.manifest.tombstones {
            out.push((EntityId::new(entity_id), ChangeOp::Delete));
        }
    }
    out
}

/// One of the side manifests' lists, concatenated: the partitions by key ascending, an item whose
/// key is already held skipped.
fn side_manifest_items<T: Clone>(
    bundle: &Bundle,
    list: impl Fn(&tessera_store::manifest::SegmentsManifest) -> &[T],
    key: impl Fn(&T) -> &str,
) -> Vec<T> {
    let mut partition_keys: Vec<&String> = bundle.partitions.keys().collect();
    partition_keys.sort();
    let mut out: Vec<T> = Vec::new();
    for partition_key in partition_keys {
        for item in list(&bundle.partitions[partition_key].manifest) {
            if !out.iter().any(|held| key(held) == key(item)) {
                out.push(item.clone());
            }
        }
    }
    out
}

/// The view groups and plain views the side manifests carry (`ingest.md` §1.3), on
/// [`side_manifest_vocabularies`]' rule: the partitions are walked by key, ascending, and a name
/// met again is skipped by the manifest merge.
fn side_manifest_view_declarations(
    bundle: &Bundle,
) -> (
    Vec<tessera_store::manifest::GroupDescriptor>,
    Vec<tessera_store::manifest::ViewDescriptor>,
) {
    (
        side_manifest_items(bundle, |m| &m.groups, |group| &group.name),
        side_manifest_items(bundle, |m| &m.plain_views, |view| &view.id),
    )
}

fn side_manifest_vocabularies(bundle: &Bundle) -> Vec<tessera_store::manifest::ManifestVocabulary> {
    side_manifest_items(bundle, |m| &m.vocabularies, |vocabulary| &vocabulary.name)
}

/// The runtime attribute columns the side manifests carry (`ingest.md` §6.3), **in one order**.
///
/// A column's position in the served list is what every buffered row, record-blob tag and
/// segment tail is positional against, so the order the manifests' lists are appended in decides
/// which column a value is read under. The partitions are a hash map; they are walked by key,
/// ascending, so two opens of one bundle build the same list. With one partition this is that
/// partition's lists; with several, every declaration is a deployment-level fact every partition
/// publishes alike, and a name met again is skipped by `Manifest::with_attributes`.
fn side_manifest_attributes(
    bundle: &Bundle,
) -> (
    Vec<tessera_store::manifest::DeclaredScalar>,
    Vec<tessera_store::manifest::ScopedScalar>,
) {
    (
        side_manifest_items(bundle, |m| &m.attributes, |d| &d.name),
        side_manifest_items(bundle, |m| &m.scoped_attributes, |f| &f.name),
    )
}

/// The live category bindings, seeded from every durable home the bundle carries
/// (per-point-attributes §3.4).
///
/// **`MANIFEST.vocabularies` plus every partition's `vocabulary_extensions`, before WAL replay.**
/// The seed's completeness is the never-reuse invariant: a draw that misses a home lands on a code
/// that already colours rows, and every functional test over fresh state still passes. This is the
/// loader half of honouring `vocabulary_extensions` — a manifest that opened and whose bindings
/// went nowhere would serve rows whose codes no key explains, and would re-mint those codes for
/// other keys.
fn initial_vocabularies_of(bundle: &Bundle) -> Result<Vocabularies> {
    let extensions: Vec<_> = bundle
        .partitions
        .values()
        .flat_map(|partition| partition.manifest.vocabulary_extensions.iter().cloned())
        .collect();
    Vocabularies::seed(
        &bundle.manifest.vocabularies,
        &bundle.manifest.declared_scalars,
        &extensions,
    )
    .map_err(|e| EngineError::Malformed(e.to_string()))
}

/// What every partition's side manifest carries, concatenated in the partitions' own order.
fn across_partitions<'a, T, I: IntoIterator<Item = T>>(
    bundle: &'a Bundle,
    of: impl Fn(&'a tessera_store::manifest::SegmentsManifest) -> I,
) -> Vec<T> {
    bundle
        .partitions
        .values()
        .flat_map(|partition| of(&partition.manifest))
        .collect()
}

/// The filter artefact of the bundle's first partition: the build's columns plus every extent that
/// partition's side manifest names.
fn open_filter_columns(
    prefix_dir: &Path,
    bundle: &Bundle,
    unfolded_attributes: &[String],
) -> std::io::Result<crate::filter::FilterColumns> {
    let partition = bundle.partitions.keys().next().cloned().unwrap_or_default();
    let manifest = bundle.partitions.get(&partition).map(|p| &p.manifest);
    crate::filter::FilterColumns::open(
        prefix_dir,
        &partition,
        &bundle.manifest.declared_scalars,
        // The scoped column families of every group, flattened: the group is already
        // the first component of each family's view ids, so what the opener needs is
        // the families and not the rosters (`views.md` §5).
        &bundle.manifest.scoped_scalars(),
        // The roster this bundle is serving, which is what places a scoped column on
        // disc (decision 0115).
        &|view: &str| bundle.manifest.incarnation_of(view),
        &bundle.manifest.vocabularies,
        manifest.map(|m| m.attr_extents.as_slice()).unwrap_or(&[]),
        // The record blob rides the same open (records §3, §7): the base the schema owes plus
        // every extent the side-manifest names — always the full shape, even while no flush
        // writes one, so a restart composes whatever was published.
        manifest.map(|m| m.record_extents.as_slice()).unwrap_or(&[]),
        manifest
            .map(|m| m.artifact_record_extents.as_slice())
            .unwrap_or(&[]),
        // The entity→term transpose rides the same open (contracts §2.4): the base the build
        // always writes plus every extent the side-manifest names, so a restart composes the
        // labels of everything flushed since the build rather than answering "unknown" for it.
        manifest
            .map(|m| m.entity_terms_extents.as_slice())
            .unwrap_or(&[]),
        manifest.map(|m| m.text_extents.as_slice()).unwrap_or(&[]),
        // The columns whose base no fold has written yet (`ingest.md` §6.3).
        unfolded_attributes,
        // Mapped, for the reason `FilterColumns::open` gives: the engine opens every
        // declared column at once and holds them for the process lifetime, so the
        // alternative is tens of GB of residency at 10⁹ paid before any filter arrives.
        true,
    )
}

impl Engine {
    /// This engine's resolved configuration.
    ///
    /// Exposed so callers need not transcribe individual fields into their own state: `/v1/meta`
    /// publishes §7.2's selection constants, and copying them into the server's `AppState` meant
    /// four more definitions, four more assignments and four more fixture lines for values the
    /// engine already holds.
    pub fn config(&self) -> &EngineConfig {
        &self.config
    }

    /// Open the bundle at `bundle_root`, replay the WAL at `wal_path`, seed the I9 allocator, and
    /// build the first [`Generation`]. `cache_dir` is the engine-local (never in-bundle) fragment
    /// cache directory (Reference Sheet R1).
    pub fn open(
        bundle_root: &Path,
        cache_dir: &Path,
        wal_path: &Path,
        plugin: impl Plugin + 'static,
        config: EngineConfig,
    ) -> Result<Engine> {
        let mut bundle = open_bundle(bundle_root).map_err(EngineError::Store)?;

        // **The attribute columns declared while the service ran, appended to the schema before
        // anything reads it** (`ingest.md` §6.3). The side manifests are the declaration's
        // durable home; `MANIFEST.json` carries the build's columns and those a fold has since
        // written. The served list is the two together, in that order, and every consumer below
        // — the vocabulary seed's widths, the record blob's field tags, the flush's writer schema,
        // `/v1/meta` — takes it from the bundle manifest. Declarations the log holds past the
        // last publication are appended after replay, below.
        // **The view groups and plain views declared while the service ran, before the roster**
        // (`ingest.md` §1.3, §10 R9): `Manifest::with_roster` drops a creation whose group the
        // manifest does not declare, so a runtime group has to be in the manifest before the
        // roster's records are applied to it, and a group-scoped column's family names a group
        // the same way.
        let (side_groups, side_plain_views) = side_manifest_view_declarations(&bundle);
        bundle.manifest = bundle
            .manifest
            .with_groups(&side_groups)
            .with_plain_views(&side_plain_views);

        // **The vocabularies declared while the service ran, before the columns that name
        // them** (`ingest.md` §1.3): the side manifests are the declaration's durable home, on
        // the attribute columns' argument, and a runtime column over a runtime vocabulary refuses
        // to seed if the vocabulary is not in the manifest by the time its width is read.
        let side_vocabularies = side_manifest_vocabularies(&bundle);
        bundle.manifest = bundle.manifest.with_vocabularies(&side_vocabularies);

        let (side_attributes, side_scoped_attributes) = side_manifest_attributes(&bundle);
        bundle.manifest = bundle
            .manifest
            .with_attributes(&side_attributes, &side_scoped_attributes);

        // **The plugin that serves a bundle must be the plugin that labelled it** (contracts
        // §2.2). The build records `data_plugin_hash` in `MANIFEST.json`; every posting in the
        // bundle is the output of *that* implementation's label rule. Serving under a different
        // one does not fail loudly anywhere downstream — the postings are read as written and the
        // requests are resolved by the new rule, so every item is mislabelled and the mislabelling
        // is invisible. This is the only place the two values meet, so it is the only place the
        // agreement can be checked.
        //
        // **Fail closed on an empty or absent manifest hash.** A bundle that does not say what
        // labelled it cannot be shown to have been labelled by this plugin, and the "unknown"
        // case is exactly the hand-written or half-migrated manifest the check is for.
        //
        // Only the *data* hash is checked. The auth module's hash keys the mask cache
        // (contracts §4.1) and is not recorded in the manifest, so there is no equivalent
        // open-time enforcement for it — and no claim here that there is.
        let served_hash = plugin.data_plugin_hash();
        if bundle.manifest.data_plugin_hash != served_hash {
            let recorded = if bundle.manifest.data_plugin_hash.is_empty() {
                "<empty>"
            } else {
                &bundle.manifest.data_plugin_hash
            };
            return Err(EngineError::Malformed(format!(
                "MANIFEST data_plugin_hash is '{recorded}' but this process serves with plugin \
                 '{served_hash}': the bundle's postings were labelled by a different rule, so \
                 serving them here would mislabel every one of them. Rebuild the bundle with \
                 this plugin, or serve it with the plugin that built it."
            )));
        }

        // **The containment partition's gate, announced where the plugin is** (see
        // `crate::containment`). The partition answers `G ⊆ M_auth` from term signatures, which is
        // sound exactly when authorisation is signature-shaped — true of the builtin plugin by
        // construction and unverifiable for any other. Under a foreign plugin nothing is built and
        // containment stays on the masked-count route, which asks `M_auth` itself. That is
        // fail-closed and correct, and it is also invisible from a response, so it is said here.
        if !crate::containment::signature_shaped(&served_hash) {
            tracing::warn!(
                data_plugin_hash = %served_hash,
                "this bundle is served by a plugin other than the builtin, so the containment \
                 partition is not built: the expression it interns is over term signatures, which \
                 is sound only where an entity's visibility is decided by its own term set, and a \
                 foreign plugin's rule cannot be shown to be. Containment is answered per artifact \
                 per request against the composed mask instead — the same answer, at the cost the \
                 partition exists to remove"
            );
        }

        let current = read_current(bundle_root)?;
        let prefix = current.prefix.clone();
        let bundle_identity = hex_decode_32(&current.manifest_digest).ok_or_else(|| {
            EngineError::Malformed(format!(
                "CURRENT manifest_digest '{}' is not 64 hex characters",
                current.manifest_digest
            ))
        })?;

        let prefix_dir = bundle_root.join(&prefix);

        // A bundle carries exactly one partition today (no compartments); take whichever one is
        // present rather than hard-coding its phash.
        let (phash, partition) = bundle
            .partitions
            .iter()
            .next()
            .map(|(k, v)| (k.clone(), v))
            .ok_or_else(|| EngineError::Malformed("bundle has no partitions".to_string()))?;

        // **The geometry version is seeded from the served filename and is process-local
        // thereafter** — bumped only by a geometry publication, never by a manifest written for
        // deny state alone (write-path §5.6). Seeding from `n` only ever moves it
        // forward, which is all `check_publishable`'s strictly-increases rule asks of it.
        let segments_version = partition.segments_n;
        let watermark = partition.manifest.watermark;

        let dict_paths: Vec<PathBuf> = partition
            .manifest
            .dict_extents
            .iter()
            .map(|e| prefix_dir.join(&e.path))
            .collect();
        let dict = Arc::new(Dict::load(&dict_paths).map_err(EngineError::Io)?);

        let postings_path = prefix_dir
            .join("partitions")
            .join(&phash)
            .join("terms")
            .join("postings.arrow");
        // Mmap-backed: the engine holds this reader for the process lifetime, so paying the
        // mmap setup cost once at open (rather than reading the whole file into memory) is the
        // right trade — see `PostingsReader::open`'s doc.
        let postings =
            Arc::new(PostingsReader::open(&postings_path, true).map_err(EngineError::Io)?);

        // **Every live delta postings tier, reopened.** A flush publishes one tier per segment
        // (§5.2) and a fragment build unions across all of them; without this an engine that
        // restarted would build every fragment from base postings alone, and every item flushed
        // since the last compaction would silently vanish from every principal's map — visible
        // before the restart, gone after it, with no error anywhere.
        //
        // **The manifest names them** (contracts §2.3 r18). The paths used to be derived from
        // `segments` with `deltas` carrying only a count, which a coalesced tier — one file
        // covering several segments' entities, sitting beside none of them — cannot be described
        // by. Every path must be digest-named in one of the two `files` maps, because
        // `open_bundle` verifies what those maps carry and nothing else: a tier reached by a path
        // the manifest names but no map covers would be served unverified.
        let mut delta_postings: Vec<Arc<DeltaTier>> = Vec::new();
        for rel in &partition.manifest.deltas {
            if !partition.manifest.files.contains_key(rel)
                && !bundle.manifest.files.contains_key(rel)
            {
                return Err(EngineError::Malformed(format!(
                    "the side-manifest lists delta tier '{rel}' which no files map digests, so \
                     opening it would serve unverified postings"
                )));
            }
            delta_postings.push(Arc::new(
                DeltaTier::open(&prefix_dir.join(rel)).map_err(EngineError::Io)?,
            ));
        }

        // The sidecar is lazy for real: nothing here is opened, mapped or verified —
        // `ExternalIdSidecar::deferred_from_manifest` only reads already-parsed JSON manifest
        // data (paths and digests), never the filesystem. No extent descriptor, digest, ordinal
        // or file path is handed to this crate — the constructor takes the manifests and the
        // prefix directory and keeps everything else behind its own API.
        let external_index = Arc::new(
            ExternalIdIndex::open(&bundle.manifest, &partition.manifest, &prefix_dir)
                .map_err(EngineError::Store)?,
        );

        // Contracts §2.6 r6: the deployment's `tessera_id` key, parsed once here and held for
        // the process lifetime. `IdentityKey::from_hex` also rejects a degenerate key — a bundle
        // this engine would otherwise open is refused rather than silently blinding identities
        // with a collapsed round schedule.
        let identity_key = IdentityKey::from_hex(&bundle.manifest.identity.key)
            .map_err(|e| EngineError::Malformed(format!("MANIFEST identity.key: {e}")))?;

        // Every piece of state that comes from durable storage — the WAL handle, the seeded I9
        // allocator, replay's overlay/buffer/`established` maps, the detached resolver state and
        // the idempotency index — is rebuilt behind **one** call, and it lives with the type that
        // owns it. Spelling it out here would put write-path reconstruction in the middle of a
        // function whose subject is the bundle.
        //
        // The side-manifest's deny state travels with it: contracts §2.3 makes a
        // `SEGMENTS-<n>.json` complete current state for its partition, and the loader honours
        // `deny` and `tombstones` (`HONOURED_STATE`), which means acting on them here. A manifest
        // that opened and whose deny state went nowhere would serve every entity it names.
        let initial_deny = initial_deny_of(&bundle);
        let mut vocabularies = initial_vocabularies_of(&bundle)?;
        // **The allocator floor comes from the side-manifest, never from the build manifest
        // alone.** Every flush raises `SegmentsManifest::entity_id_high_water` past the ids it
        // consumed, while `MANIFEST.json`'s value is frozen at build — and a fold *lowers* it, to
        // the snapshot's bound, for a different reader. Seeding from the build value is safe only
        // for as long as the WAL still carries the `Lease` and `IngestBatch` records
        // `high_water_from` derives the rest from — and rotation deletes exactly those. The rule
        // is `alloc::allocator_floor`, named rather than spelled out here so the property test
        // can exercise it instead of restating it: an id handed out twice grants the new item
        // every access the old one had.
        let side_manifest_high_waters: Vec<u64> =
            across_partitions(&bundle, |m| [m.entity_id_high_water]);
        // **The row-less mark's homes are the side manifests only**, and `SEGMENTS-0.json` is one
        // of them — a build whose declaration carries layers spends row-less ids and records the mark there, so
        // this is where a built layer's claim is honoured. `MANIFEST.json` carries no such field at
        // all, and folding the ceiling in as the bundle term is what says "nothing row-less yet"
        // without inventing one.
        let side_manifest_low_waters: Vec<u64> =
            across_partitions(&bundle, |m| [m.entity_id_low_water]);
        // One partition today, so this concatenation is the whole registry; at more than one it is
        // the union, and a layer registered against one partition is a layer of the deployment
        // (⊘ **I13b's obligation lands here at the first second partition** — the registry itself
        // is partition-independent, but nothing yet checks that two partitions agree about a name).
        let manifest_layers: Vec<tessera_types::layer::RegisteredLayer> =
            across_partitions(&bundle, |m| m.layers.iter().cloned());
        let manifest_layer_tombstones: Vec<String> =
            across_partitions(&bundle, |m| m.layer_tombstones.iter().cloned());
        // **The roster's runtime half, unioned on `manifest_layers`' argument** (`views.md` §3.2):
        // a view is a deployment-level object — its key is the group's, not a partition's — so
        // the creations and the tombstones belong to the deployment whichever
        // partition's manifest published them. With one partition this is that partition's list.
        let manifest_created_views: Vec<tessera_types::view::CreatedView> =
            across_partitions(&bundle, |m| m.views.iter().cloned());
        let manifest_dead_incarnations: Vec<tessera_types::view::DeadIncarnation> =
            across_partitions(&bundle, |m| m.dead_view_incarnations.iter().cloned());
        // The views the *build* declared, whose keys a create must not reissue.
        let declared_views: Vec<(String, String)> = bundle
            .manifest
            .groups
            .iter()
            .flat_map(|group| {
                group
                    .views
                    .iter()
                    .map(|view| (group.name.clone(), view.key.clone()))
            })
            .collect();
        // **The union across partitions, on `manifest_layers`' argument**: an artifact is a
        // deployment-level object with an entity of its own, so its membership belongs to the
        // deployment rather than to whichever partition's manifest happens to name the extent.
        // With one partition this is that partition's list.
        let manifest_membership_extents: Vec<tessera_store::manifest::MembershipExtent> =
            across_partitions(&bundle, |m| m.membership_extents.iter().cloned());
        // The two lists that make a level's derived structures placeable across a restart, unioned
        // on the same argument.
        let manifest_level_versions: Vec<tessera_store::manifest::LevelVersion> =
            across_partitions(&bundle, |m| m.level_versions.iter().cloned());
        let manifest_derived_extents: Vec<tessera_store::manifest::DerivedExtent> =
            across_partitions(&bundle, |m| m.derived_extents.iter().cloned());
        let (overlay, buffer, write_state) = WritePath::reconstruct(
            wal_path,
            crate::write::ManifestSeed {
                high_water: tessera_lifecycle::alloc::allocator_floor(
                    bundle.manifest.entity_id_high_water,
                    &side_manifest_high_waters,
                ),
                low_water: tessera_lifecycle::alloc::allocator_ceiling(
                    tessera_types::layer::ROWLESS_CEILING,
                    &side_manifest_low_waters,
                ),
                layers: &manifest_layers,
                tombstones: &manifest_layer_tombstones,
                created_views: &manifest_created_views,
                dead_view_incarnations: &manifest_dead_incarnations,
                declared_views,
                // **The `members` expansion, so replay's `ViewDrop` arm prunes every id the key
                // names** (`views.md` §3.3, decision 0115). The owner's spelling alone would
                // leave a sharing group's buffered rows in the log to be flushed into whatever
                // takes the key next.
                view_ids_of_key: &|group: &str, key: &str| {
                    bundle.manifest.view_ids_for_key(group, key)
                },
                membership_extents: &manifest_membership_extents,
                level_versions: &manifest_level_versions,
                prefix_dir: prefix_dir.clone(),
                manifest: &bundle.manifest,
                attributes: crate::attributes::RuntimeAttributes::seed(
                    side_attributes.clone(),
                    side_scoped_attributes.clone(),
                ),
                vocabularies: crate::vocabularies::RuntimeVocabularies::seed(
                    side_vocabularies.clone(),
                ),
                view_declarations: crate::view_declarations::RuntimeViewDeclarations::seed(
                    side_groups.clone(),
                    side_plain_views.clone(),
                ),
            },
            &dict,
            &initial_deny,
            &mut vocabularies,
            // **Does this row's own view hold it**, not "does any view" (`views.md` §4). An
            // entity may hold a row in several views at once — that is what the ingest join
            // produces — so a predicate over the entity alone would discard a pending row of a
            // second view because the first had already been flushed, leaving it in no segment
            // and no buffer.
            |entity, view| {
                bundle.partitions.values().any(|partition| {
                    partition
                        .views
                        .get(view)
                        .is_some_and(|data| data.row_space.row_of(entity).is_some())
                })
            },
        )?;

        // The vocabularies and attributes the log holds past the last publication.
        let runtime_vocabularies = write_state.vocabularies.snapshot(&vocabularies);
        bundle.manifest = bundle.manifest.with_vocabularies(&runtime_vocabularies);
        let (runtime_attributes, runtime_scoped_attributes) = write_state.attributes.snapshot();
        let unfolded_attributes = write_state.attributes.entity_names();
        let plugin: Arc<dyn Plugin> = Arc::new(plugin);
        let auth_plugin_hash = hex_decode_32(&plugin.auth_plugin_hash()).ok_or_else(|| {
            EngineError::Malformed("plugin auth_plugin_hash is not 64 hex characters".to_string())
        })?;

        let fragment_cache = Arc::new(FragmentCache::new(
            cache_dir,
            bundle_identity,
            auth_plugin_hash,
        ));

        // D-D: build the shared compute pool now, not lazily on first request — a pool that
        // cannot be built is an `Engine` that cannot serve any viewport, and that is a fact about
        // this engine's *open*-time health, not a fact to discover on whichever request happens
        // to be first (fail-closed: this engine simply does not open).
        let pool = Arc::new(
            build_compute_pool(config.compute_threads)
                .map_err(|e| EngineError::ThreadPoolBuild(e.to_string()))?,
        );

        // `Arc`-wrapped from the start: the `Engine` and its `WritePath` share this one
        // pointer, so a swap published by an acceptance is the swap every read path observes.
        // **The bundle as the roster makes it** (`views.md` §3.2): the views a build declared,
        // plus every view created while the service ran and replayed just now, minus every key
        // dropped. Applied here, before the first generation is built, because everything below
        // reads the manifest — the deny mask over every view, `/v1/meta`, view resolution on both
        // planes — and a created view absent from it comes back from a restart as a 404.
        // **And the groups and plain views the log holds past the last publication**, before the
        // roster below is applied to the manifest, on the merge's own rule above.
        let (runtime_groups, runtime_plain_views) = write_state.view_declarations.snapshot();
        let (created_views, dead_incarnations) = write_state.roster.snapshot();
        // **And the group-scoped columns a flush wrote** (`views.md` §5).
        // `scoped_scalars[..].views` names the views that have a column; a flush of a view created
        // since the build wrote one, and `SegmentsManifest::scoped_columns` is where that survives
        // a restart — `MANIFEST.json` being rewritten only by a fold.
        // The incarnation travels with the pair: a column of a dead incarnation is on disc under
        // the same path a key created again would use, and `with_scoped_columns` drops it rather
        // than publishing the predecessor's values as the new view's (decision 0115).
        let scoped_columns: Vec<(String, String, tessera_types::view::ViewIncarnation)> =
            across_partitions(&bundle, |m| {
                m.scoped_columns
                    .iter()
                    .map(|c| (c.column.clone(), c.view.clone(), c.incarnation))
            });
        let bundle = if created_views.is_empty()
            && dead_incarnations.is_empty()
            && scoped_columns.is_empty()
            && runtime_attributes.is_empty()
            && runtime_scoped_attributes.is_empty()
            && runtime_groups.is_empty()
            && runtime_plain_views.is_empty()
        {
            Arc::new(bundle)
        } else {
            // The groups and plain views first, so a creation the log holds for a group the log
            // also declared lands on a group the manifest carries; then the runtime columns, so a
            // scoped family the log declared has its list to extend when the pairs a flush wrote
            // for it are applied.
            let manifest = bundle
                .manifest
                .with_groups(&runtime_groups)
                .with_plain_views(&runtime_plain_views)
                .with_attributes(&runtime_attributes, &runtime_scoped_attributes)
                .with_roster(&created_views, &dead_incarnations)
                .with_scoped_columns(&scoped_columns);
            Arc::new(bundle).with_views(manifest)
        };

        // The filter artefact belongs to the published prefix, so it is opened here with the
        // bundle and carried forward by every generation successor. The build's column plus every
        // extent the partition's side-manifest names: a restart therefore composes exactly what the
        // flushes before it published, rather than serving the build's coverage and answering short
        // over everything ingested since (`filter-index.md` §2.1).
        let filter_columns = {
            let partition = bundle.partitions.keys().next().cloned().unwrap_or_default();
            Arc::new(
                open_filter_columns(&prefix_dir, &bundle, &unfolded_attributes).map_err(|e| {
                    EngineError::Store(tessera_store::StoreError::Io {
                        path: prefix_dir.join("partitions").join(&partition),
                        source: e,
                    })
                })?,
            )
        };

        // **The suggestion indexes, built here and synchronously** (`value-suggestion.md` §6.1).
        //
        // At open rather than lazily because a first keystroke that paid the whole sort would be a
        // request-shaped cold start; on the pool because the sort *is* the build cost — 34–38 s
        // single-threaded for 22M entries at 10⁷ values, against 2.4–4.2 s to write the
        // dictionary. Into the engine's own cache directory, which is where a derived, undigested,
        // rebuilt-every-open file belongs: contracts §2.1 fixes what a bundle contains, and this
        // is not part of it.
        //
        // **Only the vocabularies a declared category column draws on.** A vocabulary nothing
        // names has no suggest surface to serve, and building an index for it would pay the sort
        // for a value set no request can reach.
        let suggest_dir = cache_dir.join(crate::suggest::SUGGEST_DIR);
        // A previous run's indexes are stale by construction — every open rebuilds — and leaving
        // them would accumulate a directory per restart under a name the next build reuses.
        let _ = std::fs::remove_dir_all(&suggest_dir);
        let suggest_names: std::collections::BTreeSet<String> = bundle
            .manifest
            .declared_scalars
            .iter()
            .filter_map(|scalar| scalar.vocabulary.clone())
            .chain(
                bundle
                    .manifest
                    .scoped_scalars()
                    .into_iter()
                    .filter_map(|family| family.vocabulary),
            )
            .collect();
        let suggest = Arc::new(crate::suggest::SuggestIndexes::build(
            &suggest_dir,
            &vocabularies,
            suggest_names,
            &pool,
        ));

        // `Generation::new` derives the deny mask, so a node restarting into a live suppression
        // set has it before its first request.
        let generation = Arc::new(ArcSwap::new(Arc::new(Generation::new(crate::GenerationParts {
            prefix,
            suggest,
            segments_version,
            watermark,
            bundle: Arc::clone(&bundle),
            dict: Arc::clone(&dict),
            postings: Arc::clone(&postings),
            fragments: fragment_cache,
            external_index,
            delta_postings,
            overlay_version: 0,
            overlay: Arc::new(overlay),
            buffer: Arc::new(buffer),
            vocabularies: Arc::new(vocabularies),
            filter_columns,
        }))));

        // Unbounded until `set_cache_bounds` is called. `tessera-server` calls it immediately
        // after `open`, having validated the figure; every other embedder (tests, benches,
        // examples) gets unbounded caches, which is what a read-only embedder wants.
        // **The fold's containment partitions, adopted where their coordinate still holds.**
        // Placed here rather than inside `reconstruct` because it is the last step of open that
        // depends on the store: the level versions it compares against are the seeded ones *plus*
        // whatever the WAL replayed over them, so it has to run after both. A partition whose
        // coordinate does not match is dropped and the level recomposes on first use — see
        // [`crate::artifacts::ArtifactProjections::adopt`], and note that the direction of the
        // mistake this forbids is permissive.
        // **The row columns' scratch, swept at open.** A composition writes its partition buckets
        // and the column itself here and removes them as it goes; what survives an open is what a
        // process that died mid-fold left behind, under a prefix nothing else in the cache
        // directory uses. Swept rather than adopted, for `TmpDir`'s reason in the build: these
        // files are meaningless outside the run that wrote them.
        let row_column_scratch = cache_dir.join(crate::artifacts::ROW_COLUMN_SCRATCH_DIR);
        let _ = std::fs::create_dir_all(&row_column_scratch);
        tessera_store::derived::sweep_row_column_scratch(&row_column_scratch);
        let artifact_projections = Arc::new(crate::artifacts::ArtifactProjections::new(
            row_column_scratch,
        ));
        artifact_projections.adopt_derived(
            &prefix_dir,
            generation.load().prefix.as_str(),
            &manifest_derived_extents,
            &write_state.artifacts,
        );
        // **What the open actually took**, counted here rather than left to be inferred from the
        // absence of a build later.
        //
        // The three lists above are written by a fold *and by `tessera build`* — the build's
        // post-bundle artifact pass files them on the same coordinates through the same writer
        // (`tessera_store::membership`), so a fresh bundle adopts exactly as a folded one does. That
        // is the half of the 2026-08-22 campaign's finding 2 this line makes observable: an open
        // reporting zero adoptions against a manifest that names extents is every coordinate being
        // rejected, which is correct and is the expensive answer — the next request derives what
        // this open would have mapped, and at half a million artifacts that derivation is the
        // minute-long one the campaign found being truncated inside a response.
        tracing::info!(
            named = manifest_derived_extents.len(),
            containment_adopted = artifact_projections.adopted(),
            prefix = %generation.load().prefix,
            "the engine adopted the prefix's derived artifact structures"
        );

        // **Every spatial level's shapes are decoded and decomposed, and every segment's piece
        // claimed or resolved and staged, before this engine serves a request**
        // (`polygon-membership.md` §6.3). The store holds what the manifests seeded plus what the
        // WAL replayed, so the shapes built here are the ones a publication would have built. The
        // pieces the build or the last fold persisted — the row-major column, or the `shape-rows`
        // row form — are claimed under the same coordinate rule as the structures adopted above;
        // what is resolved is the segments no persisted form covers, the flushed ones. The row
        // forms built below take the staged pieces. The cost is reported: it is the open's, and it
        // is the figure stage 2 measures.
        let shapes = Arc::new(crate::shapes::ShapeStore::new());
        {
            let (layers, _) = write_state.registry.snapshot();
            let warmed = shapes.warm(
                &generation.load().bundle,
                &layers,
                &write_state.artifacts,
                &crate::shapes::PersistedPieces {
                    prefix_dir: Some(&prefix_dir),
                    extents: &manifest_derived_extents,
                },
            );
            if warmed.levels > 0 {
                tracing::info!(
                    levels = warmed.levels,
                    artifacts = warmed.artifacts,
                    pieces_claimed = warmed.pieces_claimed,
                    pieces_resolved = warmed.pieces_resolved,
                    held_claimed = warmed.held_claimed,
                    held_decomposed = warmed.held_decomposed,
                    rows_tested = warmed.rows_tested,
                    build_ms = warmed.build_ms,
                    claim_ms = warmed.claim_ms,
                    resolve_ms = warmed.resolve_ms,
                    held_bytes = warmed.held_bytes,
                    elapsed_ms = warmed.elapsed_ms,
                    "the engine built every spatial level's shapes and claimed or resolved every \
                     segment's piece"
                );
            }
        }

        let row_projection_cache = Arc::new(RowProjectionCache::new(u64::MAX));
        let region_cache = Arc::new(crate::single_flight::SingleFlightCache::new(u64::MAX));
        let refresh_in_flight = Arc::new(AtomicU64::new(crate::refresh::NO_REFRESH));
        let refresh_enabled = Arc::new(AtomicBool::new(true));
        let refresh_paused = Arc::new(AtomicBool::new(false));
        let coalesce_enabled = Arc::new(AtomicBool::new(true));
        let merge_enabled = Arc::new(AtomicBool::new(true));
        let fold_paused = Arc::new(AtomicBool::new(false));
        let fold_publication_paused = Arc::new(AtomicBool::new(false));
        let merge_publication_paused = Arc::new(AtomicBool::new(false));
        // **Bounded from construction**, unlike the caches `tessera_server::prepare` bounds after
        // `open`: the memo's entries are 512 B and its live set is one ladder per (session, view),
        // so there is no figure a deployment would set. What the bound answers is the superseded
        // part — see `occupancy::DEFAULT_OCCUPANCY_CACHE_BYTES`.
        let occupancy = Arc::new(crate::single_flight::SingleFlightCache::new(
            crate::occupancy::DEFAULT_OCCUPANCY_CACHE_BYTES,
        ));
        let occupancy_walks = Arc::new(AtomicU64::new(0));
        let stage = crate::stage::StageDeps {
            walks: Arc::clone(&occupancy_walks),
            occupancy: Arc::clone(&occupancy),
            pool: Arc::clone(&pool),
            enabled: Arc::new(AtomicBool::new(true)),
            in_flight: Arc::new(std::sync::Mutex::new(FxHashMap::default())),
        };

        let engine = Engine {
            generation: Arc::clone(&generation),
            plugin,
            // Unbounded until `set_cache_bounds` is called. `tessera-server` calls it immediately
            // after `open`, having validated the figure; every other embedder (tests, benches,
            // examples) gets unbounded caches, which is what a read-only embedder wants.
            row_projection_cache: Arc::clone(&row_projection_cache),
            region_cache: Arc::clone(&region_cache),
            max_region_cells: AtomicU64::new(crate::region::DEFAULT_MAX_REGION_CELLS as u64),
            artifact_projections: Arc::clone(&artifact_projections),
            shapes: Arc::clone(&shapes),
            masked_counts: Arc::new(crate::histogram::MaskedCountCache::default()),
            occupancy: Arc::clone(&occupancy),
            derived_geometry: Arc::new(crate::derived_cache::DerivedCache::default()),
            suggest_sets: Arc::new(crate::suggest_set::SuggestSets::default()),
            lineages: Arc::new(crate::cut::Lineages::new()),
            level_contents: Arc::new(crate::artifact_content::LevelContents::new()),
            pool,
            bundle_root: bundle_root.to_path_buf(),
            suggest_dir,
            config,
            next_token_id: AtomicU64::new(0),
            write: WritePath::new(write_state),
            identity_key,
            boot_nonce: OsRng.next_u64(),
            serial_fallback_max_rows: AtomicU64::new(crate::viewport::SERIAL_FALLBACK_MAX_ROWS),
            filter_crossings_projected: AtomicU64::new(0),
            filter_crossings_per_tile: AtomicU64::new(0),
            member_of_column_walks: AtomicU64::new(0),
            filter_row_routed: AtomicU64::new(0),
            stale_serves: AtomicU64::new(0),
            refreshes: Arc::new(AtomicU64::new(0)),
            refresh_in_flight: Arc::clone(&refresh_in_flight),
            refresh_enabled: Arc::clone(&refresh_enabled),
            refresh_paused: Arc::clone(&refresh_paused),
            coalesce_enabled: Arc::clone(&coalesce_enabled),
            merge_enabled: Arc::clone(&merge_enabled),
            fold_paused: Arc::clone(&fold_paused),
            fold_publication_paused: Arc::clone(&fold_publication_paused),
            merge_publication_paused: Arc::clone(&merge_publication_paused),
            full_projection_builds: AtomicU64::new(0),
            projection_routes: Arc::new(crate::compose::ProjectionRoutes::default()),
            occupancy_walks: Arc::clone(&occupancy_walks),
            stage,
        };

        // **Every level's row form, built before this engine serves a request** — the same rule
        // the shapes above follow, one structure along, and for a cost an order larger: rung 3's
        // `mesh/descriptors` projection is a *measured* 23.3 s over a 1.66×10⁹-row membership, and
        // left lazy it landed on whichever request of a fresh process arrived first. See
        // `Engine::warm_artifact_projections` for what it does and does not build.
        let warmed = engine.warm_artifact_projections();
        if warmed.levels > 0 {
            tracing::info!(
                levels = warmed.levels,
                elapsed_ms = warmed.elapsed_ms,
                builds = engine.artifact_projections.builds(),
                "the engine built every level's artifact row form; no request pays for one"
            );
        }
        // The pieces the shape warm staged were taken by the builds above; a level nothing built
        // a form for — a suppressed layer's — would otherwise hold its pieces for the process's
        // life.
        engine.shapes.clear_staged();
        Ok(engine)
    }

    /// Publish a new row-space geometry. **The outgoing one is not retained**: nothing holds it
    /// but the requests already in flight against it, each through the `Arc` it loaded at its
    /// start, and it is freed when the last of those completes (`geometry-pinning.md` §1).
    ///
    /// **The single seam a geometry swap may go through**, and the only thing in this process that
    /// moves `segments_version`. A flush is its production caller.
    ///
    /// **It structurally cannot regress authorisation state.** `overlay` and `buffer` are carried
    /// forward from whatever generation is live at the instant of the swap — this method has no
    /// parameter that could carry a stale one, which is what lets it exist as a public API at all.
    /// The one thing that may change them is `PrefixRotation::retired`, Rule F's retirement, and it
    /// only ever *withdraws* deletions.
    ///
    /// `overlay_version` moves with that and with nothing else. Bumping it on a geometry-only swap
    /// would falsely signal a change on lifecycle §1.2's *security-state* axis, which §8.5's cache
    /// keys read; **not** bumping it on a retirement would leave a real change to that state
    /// invisible to the same keys.
    ///
    /// **What it swaps, and what it carries forward.** Everything a [`GeometryPublication`] names,
    /// plus — where the publication carries a `PrefixRotation` — the base postings, the bundle
    /// identity and the fragment cache it keys, and the external-id sidecar. Those four used to be
    /// bound at [`Engine::open`] for the process lifetime, which made this a **compaction-shaped**
    /// publication in the narrow sense that it presumed the term index and dictionary were
    /// unchanged (§11.3 as it then read) — precisely the premise a fold breaks (decision 0050). It
    /// no longer presumes it: a rotation is expressible here, and a publication that does not carry
    /// one carries those four forward from the live generation unchanged.
    ///
    /// **Publication happens on the write executor, and this is a submission to it.** That is
    /// what makes "one publisher" structural rather than a discipline (`write.rs`'s module doc;
    /// `scripts/check-layers.sh` rule 1). It used to swap the pointer here, under a
    /// compare-and-swap — safe against another caller of this method, but not against the
    /// executor's own unconditional `store`, which could lose the publication entirely. The window
    /// was narrowed by re-reading the identity and never closed. It is closed now: there is one
    /// publisher and nothing to race. Closes #59.
    ///
    /// Blocks until the executor has performed the swap, so a returned `Ok` means the geometry is
    /// live — the same promise a `Receipt` carries for a lifecycle command.
    pub fn publish_geometry(
        &self,
        publication: GeometryPublication,
    ) -> std::result::Result<(), PublishGeometryError> {
        self.write.publish_geometry(publication)
    }

    /// Whether any partition of the live bundle is serving an older `SEGMENTS-<n>.json` than the
    /// newest one present — [`tessera_store::PartitionData::stepped_down`], across the bundle.
    ///
    /// **A stepped-down candidate carries no deny state** (`UnverifiedDenyManifest` refuses those
    /// outright), so what a step-down costs is *items*, not re-exposure. For a read-only replica
    /// that is fail-safe staleness. For a node that **writes** it is not: the ingest buffer is
    /// reconstructed as the WAL rows at or above the *served* watermark, so after a rotation the
    /// rows between an older manifest's watermark and the newest one's are gone, and re-flushing
    /// from a stepped-down watermark would silently lose them.
    ///
    /// Every node today is a writing node — `tessera-server` starts the write executor
    /// unconditionally — so `readyz` fails on this unconditionally. Lifecycle §6's reader/writer
    /// distinction is what would make a qualifier meaningful, and it does not exist.
    pub fn any_partition_stepped_down(&self) -> bool {
        self.generation
            .load()
            .bundle
            .partitions
            .values()
            .any(|p| p.stepped_down())
    }

    /// The live generation.
    ///
    /// **A request must load this exactly once, at its start** (lifecycle §1.1): resolving a
    /// session's terms against one generation's dictionary and then building its fragment against
    /// a later generation's watermark is the cross-generation mismatch every cache key in this
    /// crate assumes cannot happen. This accessor returns an owned snapshot precisely so a caller
    /// cannot accidentally take two.
    pub fn generation(&self) -> Arc<Generation> {
        self.generation.load_full()
    }

    /// The plugin this engine was opened with — the `/control/ingest` handler calls
    /// `terms_of_labels` through this to turn an item's `access` labels into descriptors.
    pub fn plugin(&self) -> &Arc<dyn Plugin> {
        &self.plugin
    }

    /// The plugin's declared sizing bounds — the ingest handler consults these to
    /// decide `over_bound`, never to exclude an item (bounds warn, never exclude — design §6.2
    /// r16).
    pub fn declared_bounds(&self) -> tessera_plugin::DeclaredBounds {
        self.plugin.declared_bounds()
    }

    /// Start this engine's write executor: move the WAL onto a dedicated thread and open the two
    /// queues every write is submitted through. **Exactly once.**
    ///
    /// ## Why this is a separate call rather than a config field or an `open` parameter
    ///
    /// `EngineConfig` is a `Copy` struct with no `Default` and no `#[non_exhaustive]`, so adding a
    /// field breaks every exhaustive literal; `Engine::open` is called with a full positional
    /// argument list. Either route makes every caller that never writes pay for the one that does.
    ///
    /// It is the better shape on its own merits, which is why it is not merely the cheaper one:
    /// **an engine that never ingests starts no thread at all**. Every test, bench, example and
    /// embedder that only reads gets exactly what it did before, and the one caller that writes
    /// says so explicitly.
    ///
    /// `&mut self` is what makes the WAL's single ownership a borrow-checker fact rather than a
    /// runtime `take` behind a lock: every caller holds the `Engine` by value before sharing it.
    pub fn start_write_executor(
        &mut self,
        queue_bound: usize,
    ) -> std::result::Result<(), crate::write::ExecutorStartError> {
        let generation = Arc::clone(&self.generation);
        let deps = self.maintenance_deps();
        self.write.start_executor(
            generation,
            Arc::clone(&self.row_projection_cache),
            queue_bound,
            deps,
            #[cfg(feature = "fault-injection")]
            None,
        )
    }

    /// The executor's maintenance dependencies, as both starters hand them over.
    fn maintenance_deps(&self) -> crate::write::MaintenanceDeps {
        crate::write::MaintenanceDeps {
            max_age_secs: self.config.flush_max_age_secs,
            max_items: self.config.flush_max_items,
            coalesce: coalesce_policy(&self.config),
            merge: merge_policy(&self.config),
            artifact_projections: Arc::clone(&self.artifact_projections),
            region_cache: Arc::clone(&self.region_cache),
            shapes: Arc::clone(&self.shapes),
            lineages: Arc::clone(&self.lineages),
            level_contents: Arc::clone(&self.level_contents),
            // The **configured** value, not the resolved policy's: compaction §4 step 3
            // re-checks write-path §7's base-segment relation against the fold's own output,
            // and `tessera-server`'s loader checks only an explicitly set one.
            configured_merge_bytes: self.config.max_merged_segment_bytes,
            suggest_dir: self.suggest_dir.clone(),
            compaction: self.config.compaction,
            coalesce_enabled: Arc::clone(&self.coalesce_enabled),
            merge_enabled: Arc::clone(&self.merge_enabled),
            fold_paused: Arc::clone(&self.fold_paused),
            fold_publication_paused: Arc::clone(&self.fold_publication_paused),
            merge_publication_paused: Arc::clone(&self.merge_publication_paused),
            refresh: crate::refresh::RefreshDeps {
                cache: Arc::clone(&self.row_projection_cache),
                pool: Arc::clone(&self.pool),
                in_flight: Arc::clone(&self.refresh_in_flight),
                refreshes: Arc::clone(&self.refreshes),
                enabled: Arc::clone(&self.refresh_enabled),
                paused: Arc::clone(&self.refresh_paused),
                projection_routes: Arc::clone(&self.projection_routes),
            },
            bundle_root: self.bundle_root.clone(),
            identity_key: self.identity_key,
            pool: Arc::clone(&self.pool),
            max_distinct_terms: self.plugin.declared_bounds().max_distinct_terms,
        }
    }

    /// As [`Engine::start_write_executor`], with a fault switchboard armed. Test builds only.
    #[cfg(feature = "fault-injection")]
    pub fn start_write_executor_with_faults(
        &mut self,
        queue_bound: usize,
        faults: Arc<tessera_lifecycle::faults::FaultSwitchboard>,
    ) -> std::result::Result<(), crate::write::ExecutorStartError> {
        let generation = Arc::clone(&self.generation);
        let deps = self.maintenance_deps();
        self.write.start_executor(
            generation,
            Arc::clone(&self.row_projection_cache),
            queue_bound,
            deps,
            Some(faults),
        )
    }
}

/// Steps 5 of compaction §4 for a prefix already committed: open it, and assemble the
/// [`PrefixRotation`](crate::geometry::PrefixRotation) that must ride the swap with it.
///
/// **Two callers, one on each side of the executor queue**, which is why this is a free function
/// rather than an `Engine` method. [`Engine::publish_rotated_prefix_for_test`] calls it and then *submits*
/// the publication; the fold's own publication (`crate::write::Executor::publish_fold`) calls it on
/// the executor thread and publishes inline, because a submission from the executor to itself is a
/// deadlock. A second copy of this sequence is how the two would come to rotate different subsets
/// of the four artefacts, which is the fail-open `PrefixRotation` exists to make unexpressible.
///
/// # Order: `CURRENT` first, and this refuses otherwise
///
/// `CURRENT` is the commit point and the only mutable file in a bundle (contracts §2.1), and the
/// bundle identity **is** the digest it names. So this reads `CURRENT`, refuses unless it names
/// `prefix`, and takes the identity from it. Publishing a prefix `CURRENT` does not name would
/// serve geometry that a restart would not find — the process and its own storage disagreeing
/// about which bundle is live, with nothing to detect it until the restart.
///
/// # Why the open skips verification
///
/// [`tessera_store::open_written_prefix`], not `open_bundle`: the caller wrote and digested every
/// one of these bytes moments ago, and re-reading tens of gigabytes to re-derive digests it already
/// computed proves nothing. That constructor's doc carries the premise and the caller obligation in
/// full — **this function's two callers are the obligation's only holders**, and each discharges it
/// by `prefix` having been written by the fold that is calling.
pub(crate) fn open_rotation(
    bundle_root: &Path,
    prefix: &str,
    live_fragments: &FragmentCache,
    retired: croaring::Bitmap,
) -> std::result::Result<(Arc<Bundle>, crate::geometry::PrefixRotation), PublishGeometryError> {
    let current = read_current(bundle_root)
        .map_err(|e| PublishGeometryError::PrefixNotOpenable(e.to_string()))?;
    if current.prefix != prefix {
        return Err(PublishGeometryError::PrefixNotCommitted {
            offered: prefix.to_string(),
            current: current.prefix,
        });
    }
    let bundle_identity = hex_decode_32(&current.manifest_digest).ok_or_else(|| {
        PublishGeometryError::PrefixNotOpenable(format!(
            "CURRENT manifest_digest '{}' is not 64 hex characters",
            current.manifest_digest
        ))
    })?;

    let prefix_dir = bundle_root.join(prefix);
    let mut bundle = tessera_store::open_written_prefix(bundle_root, prefix)
        .map_err(|e| PublishGeometryError::PrefixNotOpenable(e.to_string()))?;
    // The columns declared while the fold ran, appended on `Engine::open`'s rule: the new
    // `MANIFEST.json` carries the schema as it stood at the plan, and the side manifest the
    // fold published carries the rest (`ingest.md` §6.3).
    let (side_attributes, side_scoped_attributes) = side_manifest_attributes(&bundle);
    let unfolded_attributes: Vec<String> = side_attributes.iter().map(|d| d.name.clone()).collect();
    bundle.manifest = bundle
        .manifest
        .with_attributes(&side_attributes, &side_scoped_attributes);
    let bundle = Arc::new(bundle);
    let (phash, partition) = bundle
        .partitions
        .iter()
        .next()
        .map(|(k, v)| (k.clone(), v))
        .ok_or_else(|| {
            PublishGeometryError::PrefixNotOpenable(
                "the written prefix has no partitions".to_string(),
            )
        })?;

    let postings = Arc::new(
        PostingsReader::open(
            &prefix_dir
                .join("partitions")
                .join(&phash)
                .join("terms")
                .join("postings.arrow"),
            true,
        )
        .map_err(|e| PublishGeometryError::PrefixNotOpenable(e.to_string()))?,
    );
    let external_index = Arc::new(
        ExternalIdIndex::open(&bundle.manifest, &partition.manifest, &prefix_dir)
            .map_err(|e| PublishGeometryError::PrefixNotOpenable(e.to_string()))?,
    );

    // **Rotated from the live one, never freshly constructed.** `FragmentCache::rotate` is what
    // carries the validated byte bound across; a `FragmentCache::new` here would silently unbound
    // the cache a deployment's startup refusal exists to bound.
    let fragments = Arc::new(live_fragments.rotate(bundle_identity));

    // **The filter columns are opened over the new prefix**, from its own manifests: the folded
    // base columns plus whatever attribute extents the publication carried forward from the
    // fold's flight. Cloning the live generation's would serve the superseded prefix's files —
    // pre-fold values, missing the blanking, missing the folded extents — and the fold is exactly
    // the publication that makes that wrong (`filter-index.md` §6.2). A declared column whose
    // files are missing refuses here rather than reading as "those entities carry no value".
    // The record blob rotates with the prefix for the reason the value columns do: the
    // fold rewrites it, and the superseded prefix's files are pre-blanking.
    let filter_columns = Arc::new(
        open_filter_columns(&prefix_dir, &bundle, &unfolded_attributes)
            .map_err(|e| PublishGeometryError::PrefixNotOpenable(e.to_string()))?,
    );

    Ok((
        bundle,
        crate::geometry::PrefixRotation {
            postings,
            fragments,
            external_index,
            filter_columns,
            retired,
        },
    ))
}

fn read_current(root: &Path) -> Result<CurrentPointer> {
    let bytes = std::fs::read(root.join("CURRENT")).map_err(EngineError::Io)?;
    serde_json::from_slice(&bytes)
        .map_err(|e| EngineError::Malformed(format!("CURRENT is not valid JSON: {e}")))
}

fn hex_decode_32(s: &str) -> Option<[u8; 32]> {
    if s.len() != 64 {
        return None;
    }
    let mut out = [0u8; 32];
    for (i, slot) in out.iter_mut().enumerate() {
        *slot = u8::from_str_radix(&s[i * 2..i * 2 + 2], 16).ok()?;
    }
    Some(out)
}

pub(crate) fn hex_encode(bytes: &[u8]) -> String {
    use std::fmt::Write;
    let mut s = String::with_capacity(bytes.len() * 2);
    for b in bytes {
        let _ = write!(s, "{b:02x}");
    }
    s
}

/// Resolve an external id to an [`EntityId`] via `tessera_store::ExternalIdSidecar` — a thin
/// newtype-free wrapper so callers in this crate keep using `EntityId` rather than the bare
/// `tessera_types`-free type the store crate returns, and so no `RunDesc`, digest, ordinal or
/// file path from the sidecar's own bookkeeping is ever named in this crate (Ruling B).
///
/// The sidecar is per-extent lazy: nothing is opened, mapped or digested until the
/// first resolution, and then only the one extent the key falls in — never the whole family.
/// This is the seam `/control/changes` resolves an external id through **at admission**,
/// authorisation-bearing because the endpoint denies whichever entity it resolves to — a wrong
/// resolution denies the wrong entity and leaves the intended target visible. WAL replay no longer
/// resolves external ids at all: the only record that needed it was deleted with `WAL_VERSION` 5
/// (decision 0048), so the resolution happens exactly once, here, before the record is written.
pub(crate) struct ExternalIdIndex(pub(crate) tessera_store::ExternalIdSidecar);

impl ExternalIdIndex {
    pub(crate) fn open(
        bundle_manifest: &tessera_store::manifest::Manifest,
        partition_manifest: &tessera_store::manifest::SegmentsManifest,
        prefix_dir: &Path,
    ) -> std::result::Result<Self, StoreError> {
        tessera_store::ExternalIdSidecar::deferred_from_manifest(
            bundle_manifest,
            partition_manifest,
            prefix_dir,
        )
        .map(ExternalIdIndex)
    }

    /// **Fallible, closing review round 4's Critical C3.** A corrupt extent, a digest mismatch or
    /// a shuffled extent list propagates as `Err(StoreError::InvalidSidecar)` through
    /// `Engine::resolve_external_id` to the handler, rather than the panic this once was — still
    /// fail-closed in effect, but no longer a panic in an async handler. *(The generic error
    /// parameter this doc used to explain existed so `tessera-lifecycle`, which cannot name
    /// `StoreError`, could take the closure at replay; replay no longer resolves external ids —
    /// decision 0048 — so only the live path remains.)*
    pub(crate) fn resolve(&self, external_id: &[u8]) -> std::result::Result<Option<EntityId>, StoreError> {
        self.0.resolve(external_id)
    }

    /// Batched form of [`Self::resolve`] — one sorted pass over `external_ids`, each extent
    /// opened at most once, rather than one open per row (`/control/ingest`'s duplicate check,
    /// contracts §3.1 r6).
    pub(crate) fn resolve_many(
        &self,
        external_ids: &[Vec<u8>],
    ) -> std::result::Result<Vec<Option<EntityId>>, StoreError> {
        self.0.resolve_many(external_ids)
    }

    /// `entity -> external_id`, distinguishing "genuinely has none" from "an inconsistency" — see
    /// `tessera_store::ExternalIdSidecar::external_id_of_checked`'s doc.
    pub(crate) fn external_id_of_checked(
        &self,
        entity: EntityId,
        high_water: u64,
    ) -> std::result::Result<Option<Vec<u8>>, StoreError> {
        self.0.external_id_of_checked(entity, high_water)
    }
}

#[cfg(test)]
mod tests {
    /// I13a pin (D-F): a panic inside `install`/`par_iter` on the engine's shared pool must
    /// propagate to the caller — never be swallowed into a truncated `Ok`. `Engine::viewport`'s
    /// parallel tile sweep runs on exactly this pool, built exactly this way (`Engine::open`'s
    /// `rayon::ThreadPoolBuilder::new().num_threads(..).build()`), via `self.pool.install(...)`;
    /// if a worker-thread panic never reached `viewport`'s caller, a panicking tile would produce
    /// a silently-truncated 200 instead of the fail-closed 500 I13a requires (the server's
    /// `JoinError` arm, already pinned by its own test — this test pins the engine-side half of
    /// that chain: the pool itself does not eat the panic before it ever reaches `spawn_blocking`).
    ///
    /// Deliberately **not** a full `Engine::open` + fixture-bundle test with an injection hook
    /// into `tile_sweep`: a `#[cfg(test)]`-visible injection point in the real per-tile path would
    /// let a test-only branch diverge from the code every real request runs. This is rayon's
    /// own propagation guarantee, pinned against the identical construction `Engine::open` uses,
    /// which is what `self.pool.install(...)` in `Engine::viewport` actually relies on.
    ///
    /// **Why this builds its own pool rather than a real `Engine`'s.** `Engine::pool` is
    /// `pub(crate)`, so an integration test in `tests/viewport.rs` cannot reach it at all, which
    /// is why this test lives here rather than there — an engine-internal `#[cfg(test)]` module is
    /// the accepted answer where `pub(crate)` visibility genuinely blocks an integration test.
    /// Going one step further
    /// — opening a real `Engine` from *inside* this module instead of building a look-alike pool
    /// — was considered and rejected as disproportionate for this one assertion: it would mean
    /// duplicating `tests/viewport.rs`'s ~100-line bundle-fixture harness (`tessera_build::build`
    /// plus Arrow-writing the points/pairs extents) into this module, or an invasive refactor
    /// to share that harness across a `tests/` integration binary and an internal `src/` module
    /// (different compilation units), for a test whose only load-bearing claim is "rayon
    /// propagates a worker panic through `install()`" — a property of rayon's own pool, not of
    /// anything `Engine::open` does when building one. The pool below is now built by `Engine::open`'s own constructor
    /// ([`super::build_compute_pool`]) rather than by a look-alike checked against it by
    /// inspection, so "identical construction" above is a fact rather than a claim.
    ///
    /// # What it does not cover, which is the half the write path uses
    ///
    /// `install` is synchronous and has a caller to propagate to. The write path's flush, merge and
    /// coalesce use `pool.spawn`, which has none, and a panic there reaches the pool's panic
    /// handler instead — so this test says nothing about them, and the reassurance it reads as was
    /// once taken for one. That path is pinned by
    /// [`a_panic_in_a_spawned_pool_task_is_recorded_before_the_process_aborts`].
    ///
    /// **Mutations this kills:** removing the panic handler's exemption for propagating APIs — if
    /// `install` ever routed through the handler, this test would abort its own process rather
    /// than pass.
    #[test]
    fn a_panic_inside_the_shared_pool_propagates_to_the_caller() {
        let pool = super::build_compute_pool(2).expect("pool should build");

        let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            pool.install(|| {
                panic!("synthetic worker-thread panic");
            })
        }));

        assert!(
            result.is_err(),
            "a panic inside install() must propagate to the caller, not be swallowed"
        );
    }

    /// **A panic in a task spawned on the shared pool leaves a record naming what panicked, before
    /// the process dies.**
    ///
    /// The write path's flush, merge and coalesce run on `pool.spawn`, whose panic rayon reports to
    /// the pool's panic handler and, with none configured, answers by aborting the process — one
    /// line, no payload, no backtrace, and `libtest` discarding the captured output of a test the
    /// runner never names. That is the "target failed, no test named" this repository has seen
    /// twice, and it is what makes a `debug_assert` inside those three passes undiagnosable.
    ///
    /// # A subprocess, because the behaviour under test ends the process
    ///
    /// The child is this same test binary, re-executed against the `#[ignore]`d case below, which
    /// runs only with `POOL_PANIC_CHILD` set — so a plain `--ignored` sweep does not abort someone's
    /// test run. What is asserted is the child's *stderr*, which is where the record has to be:
    /// `libtest` captures the print macros and drops what it captured when the process dies, so a
    /// record written through them would be exactly as lost as the panic message it replaces.
    ///
    /// **Mutations this kills** (each run): dropping the `panic_handler` from
    /// [`super::build_compute_pool`] — the child aborts with rayon's own line and no payload;
    /// writing the record through `eprintln!` rather than to `stderr` directly — the child aborts
    /// with nothing; dropping the payload or the thread from [`super::describe_pool_panic`] — the
    /// corresponding assertion fails.
    #[cfg(unix)]
    #[test]
    fn a_panic_in_a_spawned_pool_task_is_recorded_before_the_process_aborts() {
        use std::os::unix::process::ExitStatusExt;

        let exe = std::env::current_exe().expect("the test binary knows its own path");
        let out = std::process::Command::new(exe)
            // A substring filter, not `--exact`: the module path of a unit test is not something
            // this test should have to restate correctly.
            .args(["--ignored", "--nocapture", "the_pool_panic_child"])
            .env(POOL_PANIC_CHILD, "1")
            .output()
            .expect("the test binary re-executes");
        let stderr = String::from_utf8_lossy(&out.stderr);

        assert!(
            !out.status.success(),
            "a panicked pooled task must not leave the process healthy: {out:?}"
        );
        assert_eq!(
            out.status.signal(),
            Some(libc::SIGABRT),
            "the behaviour is unchanged — the process still aborts; only the record is new: \
             {stderr}"
        );
        assert!(
            stderr.contains("spawned on the shared compute pool panicked"),
            "the record names the subsystem: {stderr}"
        );
        assert!(
            stderr.contains("synthetic pooled-task panic"),
            "the record carries the payload, which is what makes it a diagnosis: {stderr}"
        );
        assert!(
            stderr.contains("worker thread:"),
            "and the thread it happened on: {stderr}"
        );
    }

    /// The child half of
    /// [`a_panic_in_a_spawned_pool_task_is_recorded_before_the_process_aborts`]. Aborts the process
    /// by design, and does nothing at all unless that parent set `POOL_PANIC_CHILD` — an
    /// `--ignored` sweep must not take a test binary down with it.
    #[test]
    #[ignore = "child half of the pool-panic test: inert unless the parent set POOL_PANIC_CHILD"]
    fn the_pool_panic_child() {
        if std::env::var_os(POOL_PANIC_CHILD).is_none() {
            return;
        }
        let pool = super::build_compute_pool(1).expect("pool should build");
        pool.spawn(|| panic!("synthetic pooled-task panic"));
        // The abort arrives on the worker thread; this one only has to still be here for it.
        std::thread::sleep(std::time::Duration::from_secs(30));
        unreachable!("the panic handler aborts long before this");
    }

    const POOL_PANIC_CHILD: &str = "TESSERA_POOL_PANIC_CHILD";

    /// **The record carries the payload for both of the shapes a panic can leave it in, and says so
    /// when it is neither.**
    ///
    /// `panic!("literal")` leaves a `&'static str` and `panic!("{x}")` a `String`; an assertion
    /// macro's payload is one of the two, and a `panic_any` is neither. A record that reads
    /// "a payload that is neither" for the common case is the diagnosis quietly not happening.
    ///
    /// **Mutations this kills:** downcasting to only one of the two types; dropping the payload
    /// from the record; dropping the backtrace.
    #[test]
    fn the_pool_panic_record_carries_every_payload_shape() {
        let from_literal: Box<dyn std::any::Any + Send> = Box::new("a literal payload");
        let from_format: Box<dyn std::any::Any + Send> =
            Box::new("a formatted payload".to_string());
        let from_neither: Box<dyn std::any::Any + Send> = Box::new(7u32);

        let literal = super::describe_pool_panic(from_literal.as_ref());
        assert!(literal.contains("a literal payload"), "{literal}");
        assert!(
            literal.contains("backtrace"),
            "the record carries a backtrace, which is the half a one-line abort never had: \
             {literal}"
        );
        assert!(super::describe_pool_panic(from_format.as_ref()).contains("a formatted payload"));
        assert!(
            super::describe_pool_panic(from_neither.as_ref()).contains("neither &str nor String")
        );
    }
}
