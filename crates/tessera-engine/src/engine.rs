//! The engine's state, the bundle open protocol and the shared compute pool.

use std::path::{Path, PathBuf};
use std::sync::atomic::AtomicU64;
use std::sync::Arc;

use arc_swap::ArcSwap;
use rand::rngs::OsRng;
use rand::RngCore;
use rustc_hash::FxHashMap;
use tessera_authz::{DeltaTier, Dict, FragmentCache, PostingsReader};
use tessera_lifecycle::Overlay;
use tessera_plugin::Plugin;
use tessera_store::{Bundle, StoreError};
use tessera_store::manifest::{CurrentPointer, Declarations};
use tessera_store::read::open_bundle;
use tessera_store::vocabulary::Vocabularies;
use tessera_types::{EntityId, IdentityKey};

use crate::{Generation, GenerationHandle};
use crate::cache::RowProjectionCache;
use crate::config::{coalesce_policy, merge_policy, EngineConfig};
use crate::error::{EngineError, Result};
use crate::geometry::GeometryPublication;
use crate::status::ServeCounters;
use crate::switches::TestSwitches;
use crate::write::{PublishGeometryError, WritePath};

/// Builds the shared compute pool with a panic handler. `install`, `join` and `scope` propagate a
/// worker panic to their caller and never reach this handler; `spawn`, used by the write path's
/// flush, merge and coalesce, has no caller to propagate to, so unhandled, rayon would abort the
/// process with one line and no payload. [`describe_pool_panic`] records the subsystem, worker
/// thread, payload and a backtrace first, to `tracing` or directly to `stderr` with no subscriber.
/// The process still aborts: a panicked task left its `in_flight` flag set, and continuing would
/// silently wedge the pass it was for the rest of the process's life.
pub(crate) fn build_compute_pool(
    threads: usize,
) -> std::result::Result<rayon::ThreadPool, rayon::ThreadPoolBuildError> {
    rayon::ThreadPoolBuilder::new()
        .num_threads(threads)
        .panic_handler(|payload| {
            let record = describe_pool_panic(payload.as_ref());
            tracing::error!(pool_panic = %record, "a task spawned on the shared compute pool panicked");
            // Not `eprintln!`: `libtest` captures the print macros and drops what it captured
            // when the process aborts below.
            let _ = std::io::Write::write_all(
                &mut std::io::stderr().lock(),
                format!("{record}\n").as_bytes(),
            );
            std::process::abort();
        })
        .build()
}

/// The record a pooled task's panic leaves: subsystem, worker thread, payload, abort-site
/// backtrace. Separate from the handler so it can be asserted without aborting the process.
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
/// pointer, plus the state that is genuinely process-lifetime — the plugin, the compute pool, the
/// `tessera_id` key, the row-projection cache and the bundle root. The dictionary, postings
/// reader, fragment cache and external-id sidecar live in [`Generation`] instead, since each
/// changes on its own publication.
pub struct Engine {
    /// The live generation pointer, shared with [`WritePath`]: the write path publishes every
    /// swap through this exact pointer and read paths load it, so a copy elsewhere would hide a
    /// swap from whichever side held it.
    pub(crate) generation: Arc<GenerationHandle>,
    pub(crate) plugin: Arc<dyn Plugin>,
    /// The row-projection cache — see [`RowProjectionCache`]'s own doc.
    pub(crate) row_projection_cache: Arc<RowProjectionCache>,
    /// A region leaf's decomposition per `(view, generation, canonical shape, stop depth)`. Not
    /// keyed on principal: rows inside the shape are tested under each request's own mask instead.
    pub(crate) region_cache: Arc<
        tessera_cache::SingleFlightCache<
            crate::region::RegionKey,
            crate::region::RegionDecomposition,
        >,
    >,
    /// `serve.max_region_cells` — the most boundary cells a region's descent may hold at one
    /// depth before it answers a cover instead.
    pub(crate) max_region_cells: AtomicU64,
    /// Artifact memberships in row space, one entry per `(view, layer, level)`. Keyed per
    /// *deployment*, unlike the session-keyed caches below, so it moves only when a layer is
    /// published.
    pub(crate) artifact_projections: Arc<crate::artifacts::ArtifactProjections>,
    /// The spatial levels' held shapes, index and per-segment resolved pieces.
    pub(crate) shapes: Arc<crate::shapes::ShapeStore>,
    /// The masked-count histograms of levels served row-major, per `(session, layer, level)`. Per
    /// *session*: it moves whenever the principal's mask does, including every accepted deny.
    pub(crate) masked_counts: Arc<crate::histogram::MaskedCountCache>,
    /// `N_occ(d)` per `(session, view, depth)` and generation, memoised so a pan at one zoom does
    /// not walk the mask again. Per *session*, since `N_occ` is counted inside one principal's own
    /// composed mask.
    pub(crate) occupancy: Arc<
        tessera_cache::SingleFlightCache<
            crate::occupancy::OccupancyKey,
            crate::occupancy::OccupiedTiles,
        >,
    >,
    /// One artifact's derived centroid, box and hull, per principal. Per *session*: the values are
    /// functions of the principal's own visible members.
    pub(crate) derived_geometry: Arc<crate::derived::cache::DerivedCache>,
    /// One session's visible values per category column, keyed on the generation and the overlay
    /// so a set taken before a suppression cannot keep offering a value with no visible members.
    pub(crate) suggest_sets: Arc<crate::suggest_set::SuggestSets>,
    /// One lineage per `(layer, level)`, keyed per *deployment* and on the store's version alone.
    pub(crate) lineages: Arc<crate::cut::Lineages>,
    /// One supplied-content table per `(layer, level)`. Keyed per *deployment*: the verdict that
    /// decides whether a viewer is served an artifact runs before this is read.
    pub(crate) level_contents: Arc<crate::artifact_content::LevelContents>,
    /// The one shared compute pool every admitted `viewport` request's tile loop `install`s onto,
    /// and the pool a segment write executes on since flush. A second pool anywhere in this crate
    /// would defeat the throttling this gives.
    pub(crate) pool: Arc<rayon::ThreadPool>,
    /// The bundle **root** — the directory holding `CURRENT` and every prefix under it, not the
    /// prefix directory: that goes stale the moment a fold publishes a new one.
    pub(crate) bundle_root: std::path::PathBuf,
    /// Where this engine writes its suggestion indexes — `<cache dir>/suggest`, engine-local and
    /// never in the bundle.
    pub(crate) suggest_dir: std::path::PathBuf,
    pub(crate) config: EngineConfig,
    pub(crate) next_token_id: AtomicU64,
    /// The write path: the WAL, the entity-id allocator, the live external-id maps, the resolver's
    /// extension state and the idempotency index.
    pub(crate) write: WritePath,
    /// The `tessera_id` blinding permutation's per-deployment key, held for the process lifetime.
    /// Never leaves the server; `IdentityKey`'s `Debug` is redacted and it has no hex accessor, so
    /// this field cannot be logged.
    pub(crate) identity_key: IdentityKey,
    /// The key records cursors are sealed under, derived from the identity key at open.
    pub(crate) cursor_key: crate::records::CursorKey,
    /// A per-process random value folded into every content key. Without it a content key could
    /// collide across a restart, since `overlay_version` restarts at zero, and a client's held
    /// declaration would be honoured against a visible set it was never computed for.
    pub(crate) boot_nonce: u64,
    /// The switches only a gated test hook writes — see [`TestSwitches`].
    pub(crate) switches: Arc<TestSwitches>,
    /// The serving counters an operator reads — see [`ServeCounters`].
    pub(crate) counters: Arc<ServeCounters>,
    /// Whether a background refresh is producing the live generation's entries. Set before the
    /// swap and cleared when the pass ends.
    pub(crate) refresh_in_flight: Arc<AtomicU64>,
    /// Every full projection build split by the route it took. Shared with the background
    /// refresh, so it does not sum to [`ServeCounters::full_projection_builds`], which counts the
    /// request path alone.
    pub(crate) projection_routes: Arc<crate::projection::ProjectionRoutes>,
    /// The occupancy stage — see [`crate::stage`]. `N_occ(d)` is memoised per depth; this fills the
    /// rest of the ladder on the pool once one request has.
    pub(crate) stage: crate::stage::StageDeps,
}

/// Every deny disposition the bundle's side-manifests carry, as the overlay a replay starts from.
/// `deny` is the current suppression set and `tombstones` names entities already deleted, unioned
/// across every partition since the overlay is engine-wide. The two stay apart: a seed that
/// unioned them would let an unsuppress retire a deletion.
///
/// A field whose bytes did not decode never reaches here — the reader refuses such a manifest
/// rather than opening it — and the `Malformed` arm below is what keeps "did not decode" from
/// becoming "nothing is denied" if it ever does.
fn initial_deny_of(bundle: &Bundle) -> Result<Overlay> {
    let mut deleted = croaring::Bitmap::new();
    let mut suppressed = croaring::Bitmap::new();
    for (key, partition) in &bundle.partitions {
        let sets = [
            (&partition.manifest.deny, &mut suppressed, "deny"),
            (&partition.manifest.tombstones, &mut deleted, "tombstones"),
        ];
        for (field, into, name) in sets {
            let entities = field.entities().ok_or_else(|| {
                EngineError::Malformed(format!(
                    "partition {key}'s side-manifest carries a {name} field that is not a \
                     base64 portable Roaring bitmap of entity ids"
                ))
            })?;
            into.or_inplace(entities);
        }
    }
    Ok(Overlay::seeded(&deleted, &suppressed))
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

/// The view groups and plain views the side manifests carry. The partitions are walked by key,
/// ascending, and a name met again is skipped by the manifest merge.
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

/// The runtime attribute columns the side manifests carry, in one order: a column's position in
/// the served list is what every buffered row, record-blob tag and segment tail is positional
/// against. Partitions are walked by key, ascending, so two opens of one bundle build the same
/// list.
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

/// The live category bindings, seeded from every durable home the bundle carries before WAL
/// replay. The seed must be complete: a draw that misses a home lands on a code that already
/// colours rows, and would re-mint that code for other keys.
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
        &bundle.manifest,
        crate::filter::PartitionExtents::of(manifest),
        // The columns whose base no fold has written yet.
        unfolded_attributes,
        // Mapped: the engine opens every declared column at once and holds it for the process
        // lifetime, so the alternative is paying tens of GB of residency before any filter arrives.
        true,
    )
    .map_err(std::io::Error::from)
}

/// What the side manifests declare, merged into the manifest and then kept: [`ManifestSeed`]
/// seeds the runtime registries from the same lists, so replay appends to what is already served.
///
/// [`ManifestSeed`]: crate::write::ManifestSeed
struct SideDeclarations {
    groups: Vec<tessera_store::manifest::GroupDescriptor>,
    plain_views: Vec<tessera_store::manifest::ViewDescriptor>,
    vocabularies: Vec<tessera_store::manifest::ManifestVocabulary>,
    attributes: Vec<tessera_store::manifest::DeclaredScalar>,
    scoped_attributes: Vec<tessera_store::manifest::ScopedScalar>,
}

/// Everything declared while the service ran, merged into the schema before anything reads it.
/// The side manifests are the declaration's durable home; `MANIFEST.json` carries the build's
/// columns and those a fold has since written.
fn merge_side_declarations(bundle: &mut Bundle) -> SideDeclarations {
    let (groups, plain_views) = side_manifest_view_declarations(bundle);
    let vocabularies = side_manifest_vocabularies(bundle);
    let (attributes, scoped_attributes) = side_manifest_attributes(bundle);
    bundle.manifest = bundle.manifest.with_declarations(&Declarations {
        groups: &groups,
        plain_views: &plain_views,
        vocabularies: &vocabularies,
        attributes: &attributes,
        scoped_attributes: &scoped_attributes,
        ..Declarations::default()
    });
    SideDeclarations {
        groups,
        plain_views,
        vocabularies,
        attributes,
        scoped_attributes,
    }
}

/// The plugin that serves a bundle must be the plugin that labelled it: every posting is that
/// implementation's output, and serving under a different one mislabels every item invisibly, with
/// no downstream failure. An empty or absent manifest hash also refuses. Only the data hash is
/// checked; the auth module's hash is not recorded in the manifest.
fn check_serving_plugin(
    manifest: &tessera_store::manifest::Manifest,
    plugin: &dyn Plugin,
) -> Result<()> {
    let served_hash = plugin.data_plugin_hash();
    if manifest.data_plugin_hash != served_hash {
        let recorded = if manifest.data_plugin_hash.is_empty() {
            "<empty>"
        } else {
            &manifest.data_plugin_hash
        };
        return Err(EngineError::Malformed(format!(
            "MANIFEST data_plugin_hash is '{recorded}' but this process serves with plugin \
             '{served_hash}': the bundle's postings were labelled by a different rule, so \
             serving them here would mislabel every one of them. Rebuild the bundle with \
             this plugin, or serve it with the plugin that built it."
        )));
    }

    // The containment partition answers `G ⊆ M_auth` from term signatures, sound only when
    // authorisation is signature-shaped, true of the builtin plugin, unverifiable for any
    // other. Under a foreign plugin containment falls back to asking `M_auth` directly per
    // request instead: correct and fail-closed, but otherwise invisible, so logged.
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
    Ok(())
}

/// One partition's base postings under a prefix. Mmap-backed: a generation holds this reader for
/// its whole life, so paying the mmap setup once beats reading the whole file into memory.
fn open_postings(prefix_dir: &Path, phash: &str) -> std::io::Result<Arc<PostingsReader>> {
    PostingsReader::open(
        &prefix_dir
            .join("partitions")
            .join(phash)
            .join("terms")
            .join("postings.arrow"),
        true,
    )
    .map(Arc::new)
}

/// Everything the live prefix is read through, opened together at [`Engine::open`]: the prefix
/// `CURRENT` names, the readers over its files, and the two values a generation carries from the
/// side manifest it was built from.
pub(crate) struct PrefixReaders {
    pub(crate) prefix: String,
    pub(crate) prefix_dir: PathBuf,
    pub(crate) bundle_identity: [u8; 32],
    /// The geometry version, seeded from the served filename and process-local thereafter: bumped
    /// only by a geometry publication, never by a manifest written for deny state alone.
    pub(crate) segments_version: u64,
    pub(crate) watermark: u64,
    pub(crate) dict: Arc<Dict>,
    pub(crate) postings: Arc<PostingsReader>,
    pub(crate) delta_postings: Vec<Arc<DeltaTier>>,
    pub(crate) external_index: Arc<ExternalIdIndex>,
    /// The deployment's `tessera_id` key. `IdentityKey::from_hex` rejects a degenerate key,
    /// refusing a bundle rather than blinding identities with a collapsed round schedule.
    pub(crate) identity_key: IdentityKey,
    /// The key records cursors are sealed under, derived from the same identity key.
    pub(crate) cursor_key: crate::records::CursorKey,
}

impl PrefixReaders {
    fn open(bundle_root: &Path, bundle: &Bundle) -> Result<PrefixReaders> {
        let current = read_current(bundle_root)?;
        let prefix = current.prefix.clone();
        let bundle_identity = hex_decode_32(&current.manifest_digest).ok_or_else(|| {
            EngineError::Malformed(format!(
                "CURRENT manifest_digest '{}' is not 64 hex characters",
                current.manifest_digest
            ))
        })?;

        let prefix_dir = bundle_root.join(&prefix);

        // One partition today; take whichever is present rather than hard-coding its phash.
        let (phash, partition) = bundle
            .partitions
            .iter()
            .next()
            .map(|(k, v)| (k.clone(), v))
            .ok_or_else(|| EngineError::Malformed("bundle has no partitions".to_string()))?;

        let dict_paths: Vec<PathBuf> = partition
            .manifest
            .dict_extents
            .iter()
            .map(|e| prefix_dir.join(&e.path))
            .collect();
        let dict = Arc::new(Dict::load(&dict_paths).map_err(EngineError::Io)?);

        let postings = open_postings(&prefix_dir, &phash).map_err(EngineError::Io)?;

        // Every live delta postings tier, reopened; without this, every item flushed since the
        // last compaction would silently vanish from every principal's map after a restart. Every
        // path must be digest-named in one of the two `files` maps, since `open_bundle` verifies
        // only what those maps carry.
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

        // Lazy for real: nothing here is opened, mapped or verified. The constructor only reads
        // already-parsed JSON manifest data (paths and digests), never the filesystem.
        let external_index = Arc::new(
            ExternalIdIndex::open(&bundle.manifest, &partition.manifest, &prefix_dir)
                .map_err(EngineError::Store)?,
        );

        let identity_key = IdentityKey::from_hex(&bundle.manifest.identity.key)
            .map_err(|e| EngineError::Malformed(format!("MANIFEST identity.key: {e}")))?;
        let cursor_key = crate::records::CursorKey::of(&bundle.manifest.identity.key);

        Ok(PrefixReaders {
            prefix,
            prefix_dir,
            bundle_identity,
            segments_version: partition.segments_n,
            watermark: partition.manifest.watermark,
            dict,
            postings,
            delta_postings,
            external_index,
            identity_key,
            cursor_key,
        })
    }
}

/// What a reconstruction of the write path leaves behind: the first generation's overlay and
/// buffer, the write-side state the engine keeps, and the category bindings replay minted into.
struct ReconstructedWrites {
    overlay: tessera_lifecycle::Overlay,
    buffer: tessera_lifecycle::IngestBuffer,
    state: crate::write::WritePathState,
    vocabularies: Vocabularies,
}

/// Every piece of write-side state from durable storage, rebuilt behind one call. The
/// side-manifest's deny state travels with it: a `SEGMENTS-<n>.json` is complete current state for
/// its partition, so a manifest that opened and whose deny state went nowhere would serve every
/// entity it names.
fn reconstruct_writes(
    wal_path: &Path,
    bundle: &Bundle,
    readers: &PrefixReaders,
    side: SideDeclarations,
) -> Result<ReconstructedWrites> {
    let initial_deny = initial_deny_of(bundle)?;
    let mut vocabularies = initial_vocabularies_of(bundle)?;
    // The allocator floor comes from the side-manifest, never the build manifest alone: every
    // flush raises `entity_id_high_water` past the ids it consumed, while `MANIFEST.json`'s
    // value is frozen at build. An id handed out twice would grant the new item every access
    // the old one had.
    let side_manifest_high_waters: Vec<u64> =
        across_partitions(bundle, |m| [m.entity_id_high_water]);
    // The row-less mark's homes are the side manifests only; `MANIFEST.json` carries no such
    // field.
    let side_manifest_low_waters: Vec<u64> = across_partitions(bundle, |m| [m.entity_id_low_water]);
    // One partition today, so this concatenation is the whole registry; at more than one it is
    // the union.
    let manifest_layers: Vec<tessera_types::layer::RegisteredLayer> =
        across_partitions(bundle, |m| m.layers.iter().cloned());
    let manifest_layer_tombstones: Vec<String> =
        across_partitions(bundle, |m| m.layer_tombstones.iter().cloned());
    // A view's key is the group's, not a partition's, so these belong to the deployment
    // whichever partition's manifest published them.
    let manifest_created_views: Vec<tessera_types::view::CreatedView> =
        across_partitions(bundle, |m| m.views.iter().cloned());
    let manifest_dead_incarnations: Vec<tessera_types::view::DeadIncarnation> =
        across_partitions(bundle, |m| m.dead_view_incarnations.iter().cloned());
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
    // The union across partitions: an artifact's membership belongs to the deployment, not
    // whichever partition's manifest happens to name the extent.
    let manifest_membership_extents: Vec<tessera_store::manifest::MembershipExtent> =
        across_partitions(bundle, |m| m.membership_extents.iter().cloned());
    // The list that makes a level's derived structures placeable across a restart.
    let manifest_level_versions: Vec<tessera_store::manifest::LevelVersion> =
        across_partitions(bundle, |m| m.level_versions.iter().cloned());
    let (overlay, buffer, state) = WritePath::reconstruct(
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
            // So replay's `ViewDrop` arm prunes every id the key names, or a sharing group's
            // buffered rows would flush into whatever takes the key next.
            view_ids_of_key: &|group: &str, key: &str| bundle.manifest.view_ids_for_key(group, key),
            membership_extents: &manifest_membership_extents,
            level_versions: &manifest_level_versions,
            prefix_dir: readers.prefix_dir.clone(),
            manifest: &bundle.manifest,
            attributes: crate::attributes::RuntimeAttributes::seed(
                side.attributes,
                side.scoped_attributes,
            ),
            vocabularies: crate::vocabularies::RuntimeVocabularies::seed(side.vocabularies),
            view_declarations: crate::view_declarations::RuntimeViewDeclarations::seed(
                side.groups,
                side.plain_views,
            ),
        },
        &readers.dict,
        &initial_deny,
        &mut vocabularies,
        // Does this row's own view hold it, not "does any view": an entity may hold a row in
        // several views at once, and a predicate over the entity alone would discard a
        // pending row of a second view already flushed for the first.
        |entity, view| {
            bundle.partitions.values().any(|partition| {
                partition
                    .views
                    .get(view)
                    .is_some_and(|data| data.row_space.row_of(entity).is_some())
            })
        },
    )?;
    Ok(ReconstructedWrites {
        overlay,
        buffer,
        state,
        vocabularies,
    })
}

/// The bundle as the log leaves it: the vocabularies, columns, groups and plain views the log
/// holds past the last publication, and the roster — the views a build declared, plus every view
/// created while the service ran, minus every key dropped. Applied before the first generation is
/// built, since a created view absent from the manifest comes back from a restart as a 404.
fn served_bundle(
    mut bundle: Bundle,
    state: &crate::write::WritePathState,
    vocabularies: &Vocabularies,
) -> Arc<Bundle> {
    let runtime_vocabularies = state.vocabularies.snapshot(vocabularies);
    let (runtime_attributes, runtime_scoped_attributes) = state.attributes.snapshot();
    let (runtime_groups, runtime_plain_views) = state.view_declarations.snapshot();
    let (created_views, dead_incarnations) = state.roster.snapshot();
    // And the group-scoped columns a flush wrote: `with_scoped_columns` drops a column of a
    // dead incarnation rather than publishing its values as the new view's.
    let scoped_columns: Vec<(String, String, tessera_types::view::ViewIncarnation)> =
        across_partitions(&bundle, |m| {
            m.scoped_columns
                .iter()
                .map(|c| (c.column.clone(), c.view.clone(), c.incarnation))
        });
    let manifest = bundle.manifest.with_declarations(&Declarations {
        groups: &runtime_groups,
        plain_views: &runtime_plain_views,
        vocabularies: &runtime_vocabularies,
        attributes: &runtime_attributes,
        scoped_attributes: &runtime_scoped_attributes,
        created_views: &created_views,
        dead_incarnations: &dead_incarnations,
        scoped_columns: &scoped_columns,
    });
    // A log that declared nothing, or vocabularies alone, keeps the bundle `open_bundle` built
    // rather than rebuilding every partition's view map to the same thing.
    if created_views.is_empty()
        && dead_incarnations.is_empty()
        && scoped_columns.is_empty()
        && runtime_attributes.is_empty()
        && runtime_scoped_attributes.is_empty()
        && runtime_groups.is_empty()
        && runtime_plain_views.is_empty()
    {
        bundle.manifest = manifest;
        Arc::new(bundle)
    } else {
        Arc::new(bundle).with_views(manifest)
    }
}

/// The generation this engine starts serving: the prefix's filter columns, this run's suggestion
/// indexes and the deny mask `Generation::new` derives, so a node restarting into a live
/// suppression set has it before its first request.
#[allow(clippy::too_many_arguments)]
fn first_generation(
    readers: &PrefixReaders,
    suggest_dir: &Path,
    bundle: &Arc<Bundle>,
    state: &crate::write::WritePathState,
    vocabularies: Vocabularies,
    overlay: tessera_lifecycle::Overlay,
    buffer: tessera_lifecycle::IngestBuffer,
    fragments: Arc<FragmentCache>,
    pool: &rayon::ThreadPool,
) -> Result<Arc<GenerationHandle>> {
    let prefix_dir = &readers.prefix_dir;
    // Opened with the bundle and carried forward by every generation successor: a restart
    // composes what the flushes before it published, not the build's coverage alone.
    let filter_columns = {
        let unfolded_attributes = state.attributes.entity_names();
        let partition = bundle.partitions.keys().next().cloned().unwrap_or_default();
        Arc::new(
            open_filter_columns(prefix_dir, bundle, &unfolded_attributes).map_err(|e| {
                EngineError::Store(tessera_store::StoreError::Io {
                    path: prefix_dir.join("partitions").join(&partition),
                    source: e,
                })
            })?,
        )
    };

    // Built synchronously at open, not lazily, so a first keystroke never pays the sort as a
    // cold start; only for the vocabularies a declared category column draws on.
    // Stale by construction; leaving a previous run's indexes would accumulate a directory
    // per restart.
    let _ = std::fs::remove_dir_all(suggest_dir);
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
        suggest_dir,
        &vocabularies,
        suggest_names,
        pool,
    ));

    Ok(Arc::new(ArcSwap::new(Arc::new(Generation::new(
        crate::GenerationParts {
            prefix: readers.prefix.clone(),
            suggest,
            segments_version: readers.segments_version,
            watermark: readers.watermark,
            bundle: Arc::clone(bundle),
            dict: Arc::clone(&readers.dict),
            postings: Arc::clone(&readers.postings),
            fragments,
            external_index: Arc::clone(&readers.external_index),
            delta_postings: readers.delta_postings.clone(),
            overlay_version: 0,
            overlay: Arc::new(overlay),
            buffer: Arc::new(buffer),
            vocabularies: Arc::new(vocabularies),
            filter_columns,
        },
    )))))
}

/// The structures a fold left under the prefix, adopted where their coordinate still holds: the
/// artifact projections and every spatial level's shapes, both warmed before this engine serves a
/// request.
fn adopt_derived_structures(
    cache_dir: &Path,
    readers: &PrefixReaders,
    bundle: &Arc<Bundle>,
    state: &crate::write::WritePathState,
) -> (
    Arc<crate::artifacts::ArtifactProjections>,
    Arc<crate::shapes::ShapeStore>,
) {
    let prefix_dir = &readers.prefix_dir;
    // Only the structures written over a view's live incarnation. A key dropped and created
    // again keeps its predecessor's structures listed until a fold, under the same view id and
    // level version, and they address rows the new view does not have.
    let incarnation_of = |view: &str| bundle.manifest.incarnation_of(view);
    let manifest_derived_extents: Vec<tessera_store::manifest::DerivedExtent> =
        across_partitions(bundle, |m| m.derived_extents.iter().cloned())
            .into_iter()
            .filter(|e| {
                crate::filter::carries_live_view(&incarnation_of, e.view.as_deref(), e.incarnation)
            })
            .collect();

    // The fold's containment partitions, adopted where their coordinate still holds; a
    // mismatched partition is dropped and the level recomposes on first use.
    // The row columns' scratch, swept at open: meaningless outside the run that wrote it.
    let row_column_scratch = cache_dir.join(crate::artifacts::ROW_COLUMN_SCRATCH_DIR);
    let _ = std::fs::create_dir_all(&row_column_scratch);
    tessera_store::derived::sweep_row_column_scratch(&row_column_scratch);
    let artifact_projections = Arc::new(crate::artifacts::ArtifactProjections::new(
        row_column_scratch,
    ));
    artifact_projections.adopt_derived(
        prefix_dir,
        &readers.prefix,
        &manifest_derived_extents,
        &state.artifacts,
    );
    // What the open actually took. An open reporting zero adoptions against a manifest that
    // names extents means every coordinate was rejected: correct but expensive, since the next
    // request then derives what this open would have mapped.
    tracing::info!(
        named = manifest_derived_extents.len(),
        containment_adopted = artifact_projections.adopted(),
        prefix = %readers.prefix,
        "the engine adopted the prefix's derived artifact structures"
    );

    // Every spatial level's shapes decoded and decomposed, and every segment's piece claimed
    // or resolved and staged, before this engine serves a request.
    let shapes = Arc::new(crate::shapes::ShapeStore::new());
    let (layers, _) = state.registry.snapshot();
    let warmed = shapes.warm(
        bundle,
        &layers,
        &state.artifacts,
        &crate::shapes::PersistedPieces {
            prefix_dir: Some(prefix_dir),
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
    (artifact_projections, shapes)
}

impl Engine {
    /// This engine's resolved configuration, exposed so `/v1/meta` can publish the selection
    /// constants directly from here.
    pub fn config(&self) -> &EngineConfig {
        &self.config
    }

    /// Open the bundle at `bundle_root`, replay the WAL at `wal_path`, seed the entity-id
    /// allocator, and build the first [`Generation`]. `cache_dir` is the engine-local (never
    /// in-bundle) fragment cache directory.
    pub fn open(
        bundle_root: &Path,
        cache_dir: &Path,
        wal_path: &Path,
        plugin: impl Plugin + 'static,
        config: EngineConfig,
    ) -> Result<Engine> {
        config.check()?;
        let mut bundle = open_bundle(bundle_root).map_err(EngineError::Store)?;
        let side = merge_side_declarations(&mut bundle);
        check_serving_plugin(&bundle.manifest, &plugin)?;

        let readers = PrefixReaders::open(bundle_root, &bundle)?;
        let ReconstructedWrites {
            overlay,
            buffer,
            mut state,
            vocabularies,
        } = reconstruct_writes(wal_path, &bundle, &readers, side)?;

        let plugin: Arc<dyn Plugin> = Arc::new(plugin);
        let auth_plugin_hash = hex_decode_32(&plugin.auth_plugin_hash()).ok_or_else(|| {
            EngineError::Malformed("plugin auth_plugin_hash is not 64 hex characters".to_string())
        })?;
        let fragment_cache = Arc::new(FragmentCache::new(
            cache_dir,
            readers.bundle_identity,
            auth_plugin_hash,
        ));
        // Built now, not lazily on first request: a pool that cannot be built is a fact about this
        // engine's open-time health, not one to discover on whichever request happens to be first.
        let pool = Arc::new(
            build_compute_pool(config.compute_threads)
                .map_err(|e| EngineError::ThreadPoolBuild(e.to_string()))?,
        );

        let bundle = served_bundle(bundle, &state, &vocabularies);
        crate::write::retire_dead_view_artifacts(
            &state.registry,
            &mut state.artifacts,
            &bundle.manifest,
        );
        // The suggestion indexes go in the engine's own cache directory, never in the bundle:
        // they are derived, and rebuilt every open.
        let suggest_dir = cache_dir.join(crate::suggest::SUGGEST_DIR);
        let generation = first_generation(
            &readers,
            &suggest_dir,
            &bundle,
            &state,
            vocabularies,
            overlay,
            buffer,
            fragment_cache,
            &pool,
        )?;
        let (artifact_projections, shapes) =
            adopt_derived_structures(cache_dir, &readers, &bundle, &state);

        let row_projection_cache = Arc::new(RowProjectionCache::new(u64::MAX));
        let region_cache = Arc::new(tessera_cache::SingleFlightCache::new(u64::MAX));
        let refresh_in_flight = Arc::new(AtomicU64::new(crate::refresh::NO_REFRESH));
        let switches = Arc::new(TestSwitches::default());
        let counters = Arc::new(ServeCounters::default());
        // Bounded from construction, unlike the caches `tessera_server::prepare` bounds after
        // `open`: entries are fixed-size, so there is no figure a deployment would set.
        let occupancy = Arc::new(tessera_cache::SingleFlightCache::new(
            crate::occupancy::DEFAULT_OCCUPANCY_CACHE_BYTES,
        ));
        let stage = crate::stage::StageDeps {
            counters: Arc::clone(&counters),
            occupancy: Arc::clone(&occupancy),
            pool: Arc::clone(&pool),
            switches: Arc::clone(&switches),
            in_flight: Arc::new(std::sync::Mutex::new(FxHashMap::default())),
        };

        let engine = Engine {
            generation: Arc::clone(&generation),
            plugin,
            // Unbounded until `set_cache_bounds` is called.
            row_projection_cache: Arc::clone(&row_projection_cache),
            region_cache: Arc::clone(&region_cache),
            max_region_cells: AtomicU64::new(crate::region::DEFAULT_MAX_REGION_CELLS as u64),
            artifact_projections: Arc::clone(&artifact_projections),
            shapes: Arc::clone(&shapes),
            masked_counts: Arc::new(crate::histogram::MaskedCountCache::default()),
            occupancy: Arc::clone(&occupancy),
            derived_geometry: Arc::new(crate::derived::cache::DerivedCache::default()),
            suggest_sets: Arc::new(crate::suggest_set::SuggestSets::default()),
            lineages: Arc::new(crate::cut::Lineages::new()),
            level_contents: Arc::new(crate::artifact_content::LevelContents::new()),
            pool,
            bundle_root: bundle_root.to_path_buf(),
            suggest_dir,
            config,
            next_token_id: AtomicU64::new(0),
            write: WritePath::new(state),
            identity_key: readers.identity_key,
            cursor_key: readers.cursor_key,
            boot_nonce: OsRng.next_u64(),
            switches,
            counters,
            refresh_in_flight: Arc::clone(&refresh_in_flight),
            projection_routes: Arc::new(crate::projection::ProjectionRoutes::default()),
            stage,
        };

        // Every level's row form, built before this engine serves a request: left lazy, the cost
        // lands on whichever request of a fresh process arrives first.
        let warmed = engine.warm_artifact_projections();
        if warmed.levels > 0 {
            tracing::info!(
                levels = warmed.levels,
                elapsed_ms = warmed.elapsed_ms,
                builds = engine.artifact_projections.builds(),
                "the engine built every level's artifact row form; no request pays for one"
            );
        }
        // A level nothing built a form for would otherwise hold its staged pieces for the
        // process's life.
        engine.shapes.clear_staged();
        Ok(engine)
    }

    /// Publish a new row-space geometry, the only seam a swap may go through and the only thing in
    /// this process that moves `segments_version`. A flush is its production caller.
    ///
    /// Cannot regress authorisation state: `overlay` and `buffer` always carry forward from
    /// whatever generation is live at the swap, since this method has no parameter that could
    /// carry a stale one; the one thing that may change them, `PrefixRotation::retired`, only ever
    /// withdraws deletions. `overlay_version` moves with that and nothing else, so cache keys that
    /// read it see every real security-state change and no false ones.
    ///
    /// Swaps everything a [`GeometryPublication`] names, plus, with a `PrefixRotation`, the base
    /// postings, bundle identity, fragment cache and external-id sidecar; without one those four
    /// carry forward unchanged. Runs on the write executor, so there is one publisher, and blocks
    /// until the swap is done.
    pub fn publish_geometry(
        &self,
        publication: GeometryPublication,
    ) -> std::result::Result<(), PublishGeometryError> {
        self.write.publish_geometry(publication)
    }

    /// Whether any partition of the live bundle is serving an older `SEGMENTS-<n>.json` than the
    /// newest one present. A stepped-down candidate carries no deny state, so the cost is items,
    /// not re-exposure — but for a writing node the ingest buffer, reconstructed from WAL rows at
    /// or above the served watermark, would silently lose rows between the served and newest
    /// watermark on re-flush. Every node today writes, so `readyz` fails on this unconditionally.
    pub fn any_partition_stepped_down(&self) -> bool {
        self.generation
            .load()
            .bundle
            .partitions
            .values()
            .any(|p| p.stepped_down())
    }

    /// The live generation. A request must load this exactly once, at its start: resolving a
    /// session's terms against one generation's dictionary and building its fragment against a
    /// later generation's watermark is a cross-generation mismatch every cache key here assumes
    /// cannot happen. Returns an owned snapshot so a caller cannot accidentally take two.
    pub fn generation(&self) -> Arc<Generation> {
        self.generation.load_full()
    }

    /// The plugin this engine was opened with — the `/control/ingest` handler calls
    /// `terms_of_labels` through this to turn an item's `access` labels into descriptors.
    pub fn plugin(&self) -> &Arc<dyn Plugin> {
        &self.plugin
    }

    /// The plugin's declared sizing bounds — the ingest handler consults these to decide
    /// `over_bound`. Bounds warn, they never exclude an item.
    pub fn declared_bounds(&self) -> tessera_plugin::DeclaredBounds {
        self.plugin.declared_bounds()
    }

    /// Start this engine's write executor: move the WAL onto a dedicated thread and open the two
    /// queues every write is submitted through. Call at most once. A separate call rather than a
    /// config field, so an engine that never ingests starts no thread at all. `&mut self` makes
    /// the WAL's single ownership a borrow-checker fact: every caller holds the `Engine` by value
    /// before sharing it.
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
            flush_max_age_secs: self.config.flush_max_age_secs,
            flush_max_items: self.config.flush_max_items,
            coalesce_policy: coalesce_policy(&self.config),
            merge_policy: merge_policy(&self.config),
            artifact_projections: Arc::clone(&self.artifact_projections),
            region_cache: Arc::clone(&self.region_cache),
            shapes: Arc::clone(&self.shapes),
            lineages: Arc::clone(&self.lineages),
            level_contents: Arc::clone(&self.level_contents),
            suggest_dir: self.suggest_dir.clone(),
            compaction: self.config.compaction,
            switches: Arc::clone(&self.switches),
            refresh: crate::refresh::RefreshDeps {
                cache: Arc::clone(&self.row_projection_cache),
                pool: Arc::clone(&self.pool),
                in_flight: Arc::clone(&self.refresh_in_flight),
                counters: Arc::clone(&self.counters),
                switches: Arc::clone(&self.switches),
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

/// Opens a prefix already committed, and assembles the
/// [`PrefixRotation`](crate::geometry::PrefixRotation) that must ride the swap with it. A free
/// function: it has two callers, one on each side of the executor queue, and a submission from
/// the executor to itself would deadlock. Reads `CURRENT` first and refuses unless it names
/// `prefix`, since publishing a prefix `CURRENT` does not name would serve geometry a restart
/// could not find. Opens with [`tessera_store::open_written_prefix`], not `open_bundle`: the
/// caller wrote and digested these bytes moments ago.
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
    // `MANIFEST.json` carries the schema as it stood at the plan, and the side manifest the fold
    // published carries the rest.
    let (side_attributes, side_scoped_attributes) = side_manifest_attributes(&bundle);
    let unfolded_attributes: Vec<String> = side_attributes.iter().map(|d| d.name.clone()).collect();
    bundle.manifest = bundle.manifest.with_declarations(&Declarations {
        attributes: &side_attributes,
        scoped_attributes: &side_scoped_attributes,
        ..Declarations::default()
    });
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

    let postings = open_postings(&prefix_dir, &phash)
        .map_err(|e| PublishGeometryError::PrefixNotOpenable(e.to_string()))?;
    let external_index = Arc::new(
        ExternalIdIndex::open(&bundle.manifest, &partition.manifest, &prefix_dir)
            .map_err(|e| PublishGeometryError::PrefixNotOpenable(e.to_string()))?,
    );

    // Rotated from the live one, never freshly constructed: `FragmentCache::rotate` carries the
    // configured byte bound across, and `FragmentCache::new` here would silently unbound it.
    let fragments = Arc::new(live_fragments.rotate(bundle_identity));

    // Opened over the new prefix, from its own manifests: cloning the live generation's would
    // serve the superseded prefix's pre-fold files, missing the blanking and the folded extents. A
    // declared column whose files are missing refuses here rather than reading as no value.
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

/// Resolve an external id to an [`EntityId`] via `tessera_store::ExternalIdSidecar`, a thin
/// wrapper so callers in this crate keep using `EntityId`. Per-extent lazy. This is the seam
/// `/control/changes` resolves an external id through at admission, and it is authorisation
/// bearing: the endpoint denies whichever entity it resolves to, so a wrong resolution denies the
/// wrong entity and leaves the intended target visible.
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

    /// Fallible: a corrupt extent, a digest mismatch or a shuffled extent list propagates as
    /// `Err(StoreError::InvalidSidecar)` through `Engine::resolve_external_id` to the handler
    /// rather than panicking, still fail-closed in effect.
    pub(crate) fn resolve(&self, external_id: &[u8]) -> std::result::Result<Option<EntityId>, StoreError> {
        self.0.resolve(external_id)
    }

    /// Batched form of [`Self::resolve`]: one sorted pass over `external_ids`, each extent opened
    /// at most once rather than one open per row.
    pub(crate) fn resolve_many(
        &self,
        external_ids: &[Vec<u8>],
    ) -> std::result::Result<Vec<Option<EntityId>>, StoreError> {
        self.0.resolve_many(external_ids)
    }

    /// `entity -> external_id`, distinguishing "genuinely has none" from "an inconsistency".
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
    /// Pins that a panic inside `install`/`par_iter` propagates to the caller rather than being
    /// swallowed: `Engine::viewport`'s tile sweep relies on that to turn a panicking tile into a
    /// 500 rather than a truncated 200. Builds its own pool, using [`super::build_compute_pool`]
    /// as `Engine::open` does, since `Engine::pool` is `pub(crate)` and unreachable from an
    /// integration test. Says nothing about `pool.spawn`, pinned instead by
    /// [`a_panic_in_a_spawned_pool_task_is_recorded_before_the_process_aborts`].
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

    /// Pins that a panic in a task spawned on the shared pool leaves a record naming what
    /// panicked, before the process aborts. Runs the child as a subprocess, gated on
    /// `POOL_PANIC_CHILD`, and asserts its stderr, since `libtest` captures the print macros and
    /// drops what it captured when the process dies.
    /// Mutations this kills: dropping the `panic_handler` from [`super::build_compute_pool`] — the
    /// child aborts with rayon's own line and no payload; writing the record through `eprintln!`
    /// rather than to `stderr` directly — the child aborts with nothing; dropping the payload or
    /// the thread from [`super::describe_pool_panic`] — the assertion fails.
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

    /// Pins that the record carries the payload for both shapes a panic can leave it in
    /// (`&'static str` or `String`), and says so plainly when it is neither.
    /// Mutations this kills: downcasting to only one of the two types; dropping the payload from
    /// the record; dropping the backtrace.
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
