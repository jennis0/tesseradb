//! The engine's state, the bundle open protocol and the shared compute pool.

use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;

use arc_swap::ArcSwap;
use mosaica_authz::{DeltaTier, Dict, FragmentCache, PostingsReader};
use mosaica_lifecycle::{ByTenancy, Overlay, Overlays};
use mosaica_store::manifest::{CurrentPointer, Declarations};
use mosaica_store::read::open_bundle;
use mosaica_store::vocabulary::Vocabularies;
use mosaica_store::Bundle;
use mosaica_types::{IdentityKey, ShardId};
use rand::rngs::OsRng;
use rand::RngCore;
use rustc_hash::FxHashMap;

use crate::cache::RowProjectionCache;
use crate::config::{coalesce_policy, merge_policy, EngineConfig};
use crate::error::{EngineError, Result};
use crate::geometry::GeometryPublication;
use crate::status::ServeCounters;
use crate::switches::TestSwitches;
use crate::write::{PublishGeometryError, WritePath};
use crate::{Generation, GenerationHandle};

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

/// The pool a level's figures are walked on ([`Engine::count_pool`]). Nothing is spawned on it, so
/// a panic reaches the `install` caller.
fn build_count_pool(
    threads: usize,
) -> std::result::Result<rayon::ThreadPool, rayon::ThreadPoolBuildError> {
    rayon::ThreadPoolBuilder::new()
        .num_threads(threads)
        .thread_name(|i| format!("mosaica-count-{i}"))
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
        "mosaica: a task spawned on the shared compute pool panicked; aborting\n  \
         worker thread: {thread}\n  payload: {message}\n  abort-site backtrace (the panic's own \
         location is on the default hook's line above):\n{}",
        std::backtrace::Backtrace::force_capture()
    )
}

/// The request-serving engine: one immutable [`Generation`] behind an atomically-swappable
/// pointer, plus the state that is genuinely process-lifetime — the compute pool, the
/// `mosaica_id` key, the row-projection and fragment caches and the bundle root. The dictionary,
/// postings reader and each shard's identity live in [`Generation`] instead, since each changes on
/// its own publication.
pub struct Engine {
    /// The live generation pointer, shared with [`WritePath`]: the write path publishes every
    /// swap through this exact pointer and read paths load it, so a copy elsewhere would hide a
    /// swap from whichever side held it.
    pub(crate) generation: Arc<GenerationHandle>,
    /// The row-projection cache — see [`RowProjectionCache`]'s own doc.
    pub(crate) row_projection_cache: Arc<RowProjectionCache>,
    /// Every shard's mask fragments, under one bound, each filed under its shard's identity.
    pub(crate) fragments: Arc<FragmentCache>,
    /// A region leaf's decomposition per `(view, generation, canonical shape, stop depth)`. Not
    /// keyed on principal: rows inside the shape are tested under each request's own mask instead.
    pub(crate) region_cache: Arc<
        mosaica_cache::SingleFlightCache<
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
    /// The figures of levels served by row: the counts over each fragment's base rows, shared by
    /// every session with the same grant, and the corrections each request applies to them
    /// ([`crate::figures`]).
    pub(crate) figures: Arc<crate::figures::FiguresCache>,
    /// `N_occ(d)` per `(session, view, depth)` and generation, memoised so a pan at one zoom does
    /// not walk the mask again. Per *session*, since `N_occ` is counted inside one principal's own
    /// composed mask.
    pub(crate) occupancy: Arc<
        mosaica_cache::SingleFlightCache<
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
    /// One viewer's cluster slots per layer and palette size ([`crate::slots`]).
    pub(crate) slots: Arc<crate::slots::SlotsCache>,
    /// One supplied-content table per `(layer, level)`. Keyed per *deployment*: the verdict that
    /// decides whether a viewer is served an artifact runs before this is read.
    pub(crate) level_contents: Arc<crate::artifact_content::LevelContents>,
    /// The shared compute pool every admitted `viewport` request's tile loop `install`s onto, and
    /// the pool a segment write executes on. Its width bounds what requests and writes use at
    /// once; `count_pool` is the one other pool.
    pub(crate) pool: Arc<rayon::ThreadPool>,
    /// Where a level's figures are walked ([`Engine::figures`]): a walk of every row of a
    /// fragment, seconds long on a large view. As wide as `pool`, so a request on `pool` never queues
    /// behind a build's chunks.
    pub(crate) count_pool: Arc<rayon::ThreadPool>,
    /// The bundle **root** — the directory holding `CURRENT` and every prefix under it, not the
    /// prefix directory: that goes stale the moment a fold publishes a new one.
    pub(crate) bundle_root: std::path::PathBuf,
    /// Where this engine writes its suggestion indexes — `<cache dir>/suggest`, engine-local and
    /// never in the bundle.
    pub(crate) suggest_dir: std::path::PathBuf,
    pub(crate) config: EngineConfig,
    pub(crate) next_token_id: AtomicU64,
    /// The write path: the WAL, the entity-id allocator, the resolver's extension state and the
    /// idempotency index.
    pub(crate) write: WritePath,
    /// The `mosaica_id` blinding permutation's key, read from the bundle's manifest and held for
    /// the process lifetime. It never leaves the server; `IdentityKey`'s `Debug` is redacted.
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

/// Every deny disposition the bundle's side-manifests carry, as the overlays a replay starts from:
/// each shard's items, and each layer space's artifacts. `deny` is the current suppression set and
/// `tombstones` names entities already deleted, unioned across every partition into the overlay
/// of the shard or space that names them. The two stay apart: a seed that unioned them would let
/// an unsuppress retire a deletion.
///
/// A field whose bytes did not decode never reaches here — the reader refuses such a manifest
/// rather than opening it — and the `Malformed` arm below is what keeps "did not decode" from
/// becoming "nothing is denied" if it ever does.
fn initial_deny_of(bundle: &Bundle) -> Result<DenyOverlays> {
    // `(deleted, suppressed)` for one target, unioned across the partitions.
    type Sets = (croaring::Bitmap, croaring::Bitmap);
    let union = |key: &str,
                 deny: &mosaica_store::manifest::EntitySet,
                 tombstones: &mosaica_store::manifest::EntitySet,
                 (deleted, suppressed): &mut Sets|
     -> Result<()> {
        for (field, into, name) in [(deny, suppressed, "deny"), (tombstones, deleted, "tombstones")]
        {
            let entities = field.entities().ok_or_else(|| {
                EngineError::Malformed(format!(
                    "partition {key}'s side-manifest carries a {name} field that is not a \
                     base64 portable Roaring bitmap of entity ids"
                ))
            })?;
            into.or_inplace(entities);
        }
        Ok(())
    };
    let mut shards = std::collections::BTreeMap::<ShardId, Sets>::new();
    let mut spaces = std::collections::BTreeMap::<mosaica_types::LayerSpace, Sets>::new();
    for (key, partition) in &bundle.partitions {
        for shard in &partition.shards {
            let record = &shard.record;
            let into = shards.entry(record.id).or_default();
            union(key, &record.deny, &record.tombstones, into)?;
        }
        for space in &partition.layer_spaces {
            let into = spaces.entry(space.space).or_default();
            union(key, &space.deny, &space.tombstones, into)?;
        }
    }
    let seeded = |(deleted, suppressed): Sets| Overlay::seeded(&deleted, &suppressed);
    Ok(DenyOverlays {
        shards: shards.into_iter().map(|(shard, sets)| (shard, seeded(sets))).collect(),
        spaces: spaces.into_iter().map(|(space, sets)| (space, seeded(sets))).collect(),
    })
}

/// One overlay per shard and one per layer space holding a deny.
struct DenyOverlays {
    shards: std::collections::BTreeMap<ShardId, Overlay>,
    spaces: std::collections::BTreeMap<mosaica_types::LayerSpace, Overlay>,
}

/// One of the side manifests' lists, concatenated: the partitions by key ascending, an item whose
/// key is already held skipped.
fn side_manifest_items<T: Clone>(
    bundle: &Bundle,
    list: impl Fn(&mosaica_store::manifest::PartitionManifest) -> &[T],
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
    Vec<mosaica_store::manifest::GroupDescriptor>,
    Vec<mosaica_store::manifest::ViewDescriptor>,
) {
    (
        side_manifest_items(bundle, |m| &m.groups, |group| &group.name),
        side_manifest_items(bundle, |m| &m.plain_views, |view| &view.id),
    )
}

fn side_manifest_vocabularies(bundle: &Bundle) -> Vec<mosaica_store::manifest::ManifestVocabulary> {
    side_manifest_items(bundle, |m| &m.vocabularies, |vocabulary| &vocabulary.name)
}

/// The runtime attribute columns the side manifests carry, in one order: a column's position in
/// the served list is what every buffered row, record-blob tag and segment tail is positional
/// against. Partitions are walked by key, ascending, so two opens of one bundle build the same
/// list.
fn side_manifest_attributes(
    bundle: &Bundle,
) -> (
    Vec<mosaica_store::manifest::DeclaredScalar>,
    Vec<mosaica_store::manifest::ScopedScalar>,
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
    of: impl Fn(&'a mosaica_store::read::PartitionData) -> I,
) -> Vec<T> {
    bundle.partitions.values().flat_map(of).collect()
}

/// The partition whose served side-manifest is the newest.
fn newest_partition(bundle: &Bundle) -> Option<&mosaica_store::read::PartitionData> {
    bundle.partitions.values().max_by_key(|p| p.segments_n)
}

/// The shard size the newest served side-manifest records. Every publication restates it, so the
/// newest is the one in force.
fn shard_size_of(bundle: &Bundle) -> u64 {
    newest_partition(bundle)
        .expect("a served bundle has a partition")
        .shard_size
}

/// The freed entity ids of `shard` the newest served side-manifest records, and the sets it holds
/// back, each split by `tenancy`. "Newest" is the partition whose served manifest has the highest
/// number, which is meaningful while a bundle has one partition, as every bundle has.
///
/// Every publication records the allocator's sets, which shrink as ids are issued and grow only at
/// a fold, so an older manifest can list an id issued since. A stepped-down partition serves a
/// manifest older than one it wrote, so nothing is taken where one is: the ids stay unissued.
fn freed_of(
    bundle: &Bundle,
    shard: ShardId,
    tenancy: &mosaica_store::tenancy::TenancyIndex,
) -> (ByTenancy, Vec<(u64, ByTenancy)>) {
    if bundle.partitions.values().any(|p| p.stepped_down()) {
        return (ByTenancy::new(), Vec::new());
    }
    let Some(record) = newest_partition(bundle)
        .and_then(|newest| newest.shard(shard))
        .map(|data| &data.record)
    else {
        return (ByTenancy::new(), Vec::new());
    };
    let free = record
        .free_entities
        .entities()
        .map(|ids| tenancy.split(ids))
        .unwrap_or_default();
    let held = record
        .held_entities
        .iter()
        .filter_map(|h| Some((h.position, tenancy.split(h.entities.entities()?))))
        .collect();
    (free, held)
}

/// What a generation reads one shard's items through beside its postings.
struct ShardIndexes {
    filter_columns: Arc<crate::filter::FilterColumns>,
    unique: Arc<mosaica_store::unique::UniqueIndexes>,
    edited: Arc<mosaica_store::edited::EditedIndex>,
}

/// The indexes of partition `phash`'s shard `record` under `prefix_dir`: the build's filter
/// columns plus every extent the record names, and the unique and edited-items runs it lists.
/// `unfolded_attributes` are the columns whose base no fold has written yet.
fn open_shard_indexes(
    prefix_dir: &Path,
    bundle: &Bundle,
    phash: &str,
    record: &mosaica_store::manifest::ShardRecord,
    unfolded_attributes: &[String],
) -> std::result::Result<ShardIndexes, mosaica_store::StoreError> {
    let partition = &bundle.partitions[phash];
    let layout = mosaica_store::ShardLayout::new(phash, record.id);
    let filter_columns = crate::filter::FilterColumns::open(
        prefix_dir,
        layout,
        &bundle.manifest,
        crate::filter::ShardExtents::of(record),
        unfolded_attributes,
        // Mapped: the engine opens every declared column at once and holds it for the process
        // lifetime, so the alternative is paying tens of GB of residency before any filter arrives.
        true,
    )
    .map_err(|e| mosaica_store::StoreError::Io {
        path: layout.dir(prefix_dir),
        source: std::io::Error::from(e),
    })?;
    let unique = mosaica_store::unique::UniqueIndexes::open(
        &bundle.manifest,
        &partition.manifest,
        record,
        prefix_dir,
        None,
    )?;
    let edited = mosaica_store::edited::EditedIndex::open(&record.edited_items, prefix_dir, None)?;
    Ok(ShardIndexes {
        filter_columns: Arc::new(filter_columns),
        unique: Arc::new(unique),
        edited: Arc::new(edited),
    })
}

/// What the side manifests declare, merged into the manifest and then kept: [`ManifestSeed`]
/// seeds the runtime registries from the same lists, so replay appends to what is already served.
///
/// [`ManifestSeed`]: crate::write::ManifestSeed
struct SideDeclarations {
    groups: Vec<mosaica_store::manifest::GroupDescriptor>,
    plain_views: Vec<mosaica_store::manifest::ViewDescriptor>,
    vocabularies: Vec<mosaica_store::manifest::ManifestVocabulary>,
    attributes: Vec<mosaica_store::manifest::DeclaredScalar>,
    scoped_attributes: Vec<mosaica_store::manifest::ScopedScalar>,
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

/// One shard's base postings under a prefix. Mmap-backed: a generation holds this reader for its
/// whole life, so paying the mmap setup once beats reading the whole file into memory.
fn open_postings(
    prefix_dir: &Path,
    layout: mosaica_store::ShardLayout<'_>,
) -> std::io::Result<Arc<PostingsReader>> {
    PostingsReader::open(&prefix_dir.join(layout.postings_rel()), true).map(Arc::new)
}

/// Everything the live prefix is read through, opened together at [`Engine::open`]: the prefix
/// `CURRENT` names, the readers over its files, and the two values a generation carries from the
/// side manifest it was built from.
pub(crate) struct PrefixReaders {
    pub(crate) prefix: String,
    pub(crate) prefix_dir: PathBuf,
    /// The partition the prefix is served from.
    pub(crate) phash: String,
    /// The geometry version, seeded from the served filename and process-local thereafter: bumped
    /// only by a geometry publication, never by a manifest written for deny state alone.
    pub(crate) segments_version: u64,
    pub(crate) dict: Arc<Dict>,
    /// Every point shard's readers, ascending by shard.
    pub(crate) shards: Vec<ShardReaders>,
    /// The deployment's `mosaica_id` key. `IdentityKey::from_hex` rejects a degenerate key,
    /// refusing a bundle rather than blinding identities with a collapsed round schedule.
    pub(crate) identity_key: IdentityKey,
    /// The key records cursors are sealed under, derived from the same identity key.
    pub(crate) cursor_key: crate::records::CursorKey,
}

/// What [`PrefixReaders`] reads one point shard through.
pub(crate) struct ShardReaders {
    pub(crate) id: ShardId,
    pub(crate) postings: Arc<PostingsReader>,
    pub(crate) delta_postings: Vec<Arc<DeltaTier>>,
    /// The shard's tenancy index, which the first generation serves and the allocator's freed ids
    /// are split by.
    pub(crate) tenancy: Arc<mosaica_store::tenancy::TenancyIndex>,
}

impl PrefixReaders {
    fn open(bundle_root: &Path, bundle: &Bundle) -> Result<PrefixReaders> {
        let current = read_current(bundle_root)?;
        let prefix = current.prefix.clone();

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

        let mut shards = Vec::with_capacity(partition.shards.len());
        for shard in &partition.shards {
            let layout = mosaica_store::ShardLayout::new(&phash, shard.record.id);
            let postings = open_postings(&prefix_dir, layout).map_err(EngineError::Io)?;

            // Every live delta postings tier, reopened; without this, every item flushed since the
            // last compaction would silently vanish from every principal's map after a restart.
            // Every path must be digest-named in one of the two `files` maps, since `open_bundle`
            // verifies only what those maps carry.
            let mut delta_postings: Vec<Arc<DeltaTier>> = Vec::new();
            for rel in &shard.record.deltas {
                if !partition.manifest.files.contains_key(rel)
                    && !bundle.manifest.files.contains_key(rel)
                {
                    return Err(EngineError::Malformed(format!(
                        "the side-manifest lists delta tier '{rel}' which no files map digests, \
                         so opening it would serve unverified postings"
                    )));
                }
                delta_postings.push(Arc::new(
                    DeltaTier::open(&prefix_dir.join(rel)).map_err(EngineError::Io)?,
                ));
            }
            let tenancy = mosaica_store::read::open_tenancy_index(
                &prefix_dir,
                &shard.record,
                mosaica_store::read::Digests {
                    side: &partition.manifest.files,
                    bundle: &bundle.manifest.files,
                },
            )
            .map_err(EngineError::Store)?;
            shards.push(ShardReaders {
                id: shard.record.id,
                postings,
                delta_postings,
                tenancy: Arc::new(tenancy),
            });
        }
        shards.sort_by_key(|shard| shard.id);

        let identity_key = IdentityKey::from_hex(&bundle.manifest.identity.key)
            .map_err(|e| EngineError::Malformed(format!("MANIFEST identity.key: {e}")))?;
        let cursor_key = crate::records::CursorKey::of(&bundle.manifest.identity.key);

        Ok(PrefixReaders {
            prefix,
            prefix_dir,
            phash,
            segments_version: partition.segments_n,
            dict,
            shards,
            identity_key,
            cursor_key,
        })
    }
}

/// What a reconstruction of the write path leaves behind: the first generation's overlays and
/// buffer, the write-side state the engine keeps, and the category bindings replay minted into.
struct ReconstructedWrites {
    deny: DenyOverlays,
    /// Each point shard's buffer; a shard with none buffered has no entry.
    buffers: std::collections::BTreeMap<ShardId, mosaica_lifecycle::IngestBuffer>,
    state: crate::write::WritePathState,
    vocabularies: Vocabularies,
}

/// Every piece of write-side state from durable storage, rebuilt behind one call. The
/// side-manifest's deny state travels with it: a `SEGMENTS-<n>.json` is complete current state for
/// its partition, so a manifest that opened and whose deny state went nowhere would serve every
/// entity it names.
fn reconstruct_writes(
    wal_path: &Path,
    several_shards: crate::write::SeveralShards,
    bundle: &Bundle,
    readers: &PrefixReaders,
    side: SideDeclarations,
) -> Result<ReconstructedWrites> {
    let DenyOverlays {
        shards: shard_deny,
        spaces: space_deny,
    } = initial_deny_of(bundle)?;
    let mut vocabularies = initial_vocabularies_of(bundle)?;
    let partition = &bundle.partitions[&readers.phash];
    let served = &partition.shards;
    let seeds: Vec<crate::write::ShardSeed> = readers
        .shards
        .iter()
        .zip(served)
        .map(|(shard, data)| {
            let (free, held) = freed_of(bundle, shard.id, &shard.tenancy);
            crate::write::ShardSeed {
                id: shard.id,
                state: data.record.state,
                high_water: data.record.high_water,
                free,
                held,
            }
        })
        .collect();
    // One partition today, so this concatenation is the whole registry; at more than one it is
    // the union.
    let manifest_layers: Vec<mosaica_types::layer::RegisteredLayer> =
        across_partitions(bundle, |p| p.manifest.layers.iter().cloned());
    let next_layer_space = across_partitions(bundle, |p| [p.manifest.next_layer_space])
        .into_iter()
        .max()
        .unwrap_or(0);
    // A view's key is the group's, not a partition's, so these belong to the deployment
    // whichever partition's manifest published them.
    let manifest_created_views: Vec<mosaica_types::view::CreatedView> =
        across_partitions(bundle, |p| p.manifest.views.iter().cloned());
    let manifest_dead_incarnations: Vec<mosaica_types::view::DeadIncarnation> =
        across_partitions(bundle, |p| {
            p.manifest.dead_view_incarnations.iter().cloned()
        });
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
    // whichever partition's manifest happens to name the extent. Each shard holds its own slice.
    let manifest_membership_extents: Vec<(ShardId, mosaica_store::manifest::MembershipExtent)> =
        across_partitions(bundle, |p| {
            p.shards.iter().flat_map(|shard| {
                let id = shard.record.id;
                shard.record.membership_extents.iter().map(move |m| (id, m.clone()))
            })
        });
    let manifest_layer_spaces: Vec<mosaica_store::manifest::LayerSpaceRecord> =
        across_partitions(bundle, |p| p.layer_spaces.iter().cloned());
    let initial_deny = Overlays {
        items: shard_deny,
        spaces: space_deny,
    };
    let (overlay, buffers, state) = WritePath::reconstruct(
        wal_path,
        several_shards,
        crate::write::ManifestSeed {
            shards: seeds,
            next_id: partition.next_shard_id,
            size: shard_size_of(bundle),
            layers: &manifest_layers,
            next_layer_space,
            registry_version: across_partitions(bundle, |p| [p.manifest.layer_registry_version])
                .into_iter()
                .max()
                .unwrap_or(0),
            created_views: &manifest_created_views,
            dead_view_incarnations: &manifest_dead_incarnations,
            declared_views,
            // So replay's `ViewDrop` arm prunes every id the key names, or a sharing group's
            // buffered rows would flush into whatever takes the key next.
            view_ids_of_key: &|group: &str, key: &str| bundle.manifest.view_ids_for_key(group, key),
            membership_extents: &manifest_membership_extents,
            layer_spaces: &manifest_layer_spaces,
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
        // Does this row's own view in its own shard hold it, not "does any view": an entity may
        // hold a row in several views at once, and a predicate over the entity alone would
        // discard a pending row of a second view already flushed for the first.
        |shard, entity, view| {
            partition.shard(shard).is_some_and(|data| {
                data.views
                    .get(view)
                    .is_some_and(|data| data.row_space.row_of(entity).is_some())
            })
        },
    )?;
    if let Some(opened) = state
        .shard_ids()
        .find(|shard| partition.shard(*shard).is_none())
    {
        return Err(crate::write::unknown_shard(
            opened,
            "opens",
            "no side-manifest holds",
        ));
    }
    Ok(ReconstructedWrites {
        deny: DenyOverlays {
            shards: overlay.items,
            spaces: overlay.spaces,
        },
        buffers,
        state,
        vocabularies,
    })
}

/// Apply the log's changes to `unique` onto each partition's unique columns and its shard's
/// indexes. The last change to a column is the one in force, and the manifest is newer than a
/// declaration it already carries an index for, so a declaration adopts its runs only where the
/// manifest has no index for the column; each run adopted is checked against the digest the log
/// recorded.
fn apply_unique_events(
    bundle: &mut Bundle,
    events: Vec<crate::write::UniqueEvent>,
    prefix_dir: &Path,
) -> Result<()> {
    let mut last: std::collections::BTreeMap<String, crate::write::UniqueEvent> =
        std::collections::BTreeMap::new();
    for event in events {
        last.insert(event.attribute.clone(), event);
    }
    for partition in bundle.partitions.values_mut() {
        for event in last.values() {
            let held = partition
                .only_shard()
                .record
                .unique_indexes
                .iter()
                .any(|runs| runs.attribute == event.attribute);
            let runs = match (event.unique, held) {
                (false, true) => None,
                (true, false) => {
                    for run in event.base.iter().chain(&event.live) {
                        let digest = mosaica_store::digest_of(&prefix_dir.join(&run.path))
                            .map_err(EngineError::Store)?;
                        if digest.size != run.size || digest.sha256 != run.sha256 {
                            return Err(EngineError::Malformed(format!(
                                "the WAL declares '{}' unique over index run '{}', whose \
                                 bytes do not match the digest the log recorded; restore the \
                                 run or the bundle it was written into",
                                event.attribute, run.path
                            )));
                        }
                        partition.manifest.files.insert(run.path.clone(), digest);
                    }
                    Some(mosaica_store::manifest::UniqueIndexRuns {
                        attribute: event.attribute.clone(),
                        base: event
                            .base
                            .iter()
                            .map(|run| mosaica_store::manifest::BaseKeyRun {
                                path: run.path.clone(),
                                first_key: run.first_key.clone(),
                                last_key: run.last_key.clone(),
                            })
                            .collect(),
                        live: event.live.iter().map(|run| run.path.clone()).collect(),
                    })
                }
                // The manifest's index is the declaration's own, or, where the side-manifests of
                // a removal and of this declaration were not written, the one from before the
                // removal. That one indexes the same flushed values, since every flush writes a
                // side-manifest and none came after it; the values buffered since are live
                // entries again after the replay.
                (true, true) => continue,
                (false, false) => continue,
            };
            let record =
                &mut std::sync::Arc::make_mut(mosaica_types::only(&mut partition.shards)).record;
            mosaica_store::unique::set_unique_index(
                &mut partition.manifest,
                record,
                &event.attribute,
                runs,
            );
        }
    }
    Ok(())
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
    let scoped_columns: Vec<(String, String, mosaica_types::view::ViewIncarnation)> =
        across_partitions(&bundle, |p| {
            let mut columns: Vec<_> = p
                .shards
                .iter()
                .flat_map(|shard| shard.record.scoped_columns.iter())
                .map(|c| (c.column.clone(), c.view.clone(), c.incarnation))
                .collect();
            columns.sort();
            columns.dedup();
            columns
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
    let manifest = match bundle.partitions.values().next() {
        Some(partition) => mosaica_store::unique::with_unique_flags(&manifest, &partition.manifest),
        None => manifest,
    };
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
    deny: DenyOverlays,
    mut buffers: std::collections::BTreeMap<ShardId, mosaica_lifecycle::IngestBuffer>,
    pool: &rayon::ThreadPool,
) -> Result<Arc<GenerationHandle>> {
    let DenyOverlays {
        shards: mut shard_deny,
        spaces,
    } = deny;
    let contents = crate::artifact_content::open_space_contents(&readers.prefix_dir, bundle)
        .map_err(|e| EngineError::Malformed(e.to_string()))?;
    let mut layer_spaces = crate::LayerSpaces::new();
    for (space, overlay) in spaces {
        layer_spaces.entry(space).or_default().overlay = Arc::new(overlay);
    }
    for (space, content) in contents {
        layer_spaces.entry(space).or_default().content = Some(content);
    }

    // The filter columns are carried forward by every generation successor: a restart composes
    // what the flushes before it published, not the build's coverage alone.
    let unfolded_attributes = state.attributes.entity_names();
    let served = &bundle.partitions[&readers.phash].shards;
    let mut shards = Vec::with_capacity(readers.shards.len());
    for (shard, data) in readers.shards.iter().zip(served) {
        let record = &data.record;
        // What replay buffered in this shard, and its edits no run holds yet.
        let buffer = buffers.remove(&shard.id).unwrap_or_default();
        let edited_live = state
            .edited_live
            .get(&shard.id)
            .cloned()
            .unwrap_or_default();
        let indexes = open_shard_indexes(
            &readers.prefix_dir,
            bundle,
            &readers.phash,
            record,
            &unfolded_attributes,
        )
        .map_err(EngineError::Store)?;
        let overlay = shard_deny.remove(&shard.id).unwrap_or_default();
        shards.push((
            shard.id,
            crate::ShardParts {
                watermark: record.watermark,
                postings: Arc::clone(&shard.postings),
                identity: record.identity,
                unique: indexes.unique,
                unique_live: Arc::new(crate::unique::UniqueLive::derive(&bundle.manifest, &buffer)),
                edited: indexes.edited,
                edited_live: Arc::new(edited_live),
                tenancy: Arc::clone(&shard.tenancy),
                delta_postings: shard.delta_postings.clone(),
                overlay_version: 0,
                overlay: Arc::new(overlay),
                buffer: Arc::new(buffer),
                filter_columns: indexes.filter_columns,
            },
        ));
    }

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
            bundle: Arc::clone(bundle),
            dict: Arc::clone(&readers.dict),
            edit_epoch: 0,
            deny_epoch: 0,
            fold_epoch: 0,
            free_epoch: 0,
            vocabularies: Arc::new(vocabularies),
            layer_spaces: Arc::new(layer_spaces),
        },
        shards,
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
    let manifest_derived_extents: Vec<(ShardId, mosaica_store::manifest::DerivedExtent)> =
        across_partitions(bundle, |p| {
            p.derived_by_shard()
                .map(|(shard, extent)| (shard, extent.clone()))
        })
        .into_iter()
        .filter(|(_, e)| {
            crate::filter::carries_live_view(&incarnation_of, e.view.as_deref(), e.incarnation)
        })
        // A dropped layer's structures stay listed until a fold.
        .filter(|(_, e)| state.registry.in_space(e.space).is_some())
        .collect();

    // The fold's containment partitions, adopted where their coordinate still holds; a
    // mismatched partition is dropped and the level recomposes on first use.
    // The row columns' scratch, swept at open: meaningless outside the run that wrote it.
    let row_column_scratch = cache_dir.join(crate::artifacts::ROW_COLUMN_SCRATCH_DIR);
    let _ = std::fs::create_dir_all(&row_column_scratch);
    mosaica_store::derived::sweep_row_column_scratch(&row_column_scratch);
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
    let layers = state.registry.snapshot();
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
        config: EngineConfig,
    ) -> Result<Engine> {
        Engine::open_with(
            bundle_root,
            cache_dir,
            wal_path,
            config,
            crate::write::SeveralShards::Refused,
        )
    }

    /// [`Engine::open`], taking writes, and a log, over a bundle of several point shards. For this
    /// crate's own tests.
    #[cfg(feature = "several-shard-writes")]
    pub fn open_writing_several_shards(
        bundle_root: &Path,
        cache_dir: &Path,
        wal_path: &Path,
        config: EngineConfig,
    ) -> Result<Engine> {
        Engine::open_with(
            bundle_root,
            cache_dir,
            wal_path,
            config,
            crate::write::SeveralShards::Written,
        )
    }

    fn open_with(
        bundle_root: &Path,
        cache_dir: &Path,
        wal_path: &Path,
        config: EngineConfig,
        several_shards: crate::write::SeveralShards,
    ) -> Result<Engine> {
        config.check()?;
        let mut bundle = open_bundle(bundle_root).map_err(EngineError::Store)?;
        let side = merge_side_declarations(&mut bundle);

        let readers = PrefixReaders::open(bundle_root, &bundle)?;
        let ReconstructedWrites {
            deny,
            buffers,
            mut state,
            vocabularies,
        } = reconstruct_writes(wal_path, several_shards, &bundle, &readers, side)?;

        let fragments = Arc::new(FragmentCache::new(cache_dir, authorisation_rule_hash()));
        // Built now, not lazily on first request: a pool that cannot be built is a fact about this
        // engine's open-time health, not one to discover on whichever request happens to be first.
        let pool = Arc::new(
            build_compute_pool(config.compute_threads)
                .map_err(|e| EngineError::ThreadPoolBuild(e.to_string()))?,
        );
        let count_pool = Arc::new(
            build_count_pool(pool.current_num_threads())
                .map_err(|e| EngineError::ThreadPoolBuild(e.to_string()))?,
        );

        apply_unique_events(
            &mut bundle,
            std::mem::take(&mut state.unique_events),
            &readers.prefix_dir,
        )?;
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
            deny,
            buffers,
            &pool,
        )?;
        let (artifact_projections, shapes) =
            adopt_derived_structures(cache_dir, &readers, &bundle, &state);

        let row_projection_cache = Arc::new(RowProjectionCache::new(u64::MAX));
        let region_cache = Arc::new(mosaica_cache::SingleFlightCache::new(u64::MAX));
        let refresh_in_flight = Arc::new(AtomicU64::new(crate::refresh::NO_REFRESH));
        let switches = Arc::new(TestSwitches::default());
        let counters = Arc::new(ServeCounters::default());
        // Bounded from construction, unlike the caches `mosaica_server::prepare` bounds after
        // `open`: entries are fixed-size, so there is no figure a deployment would set.
        let occupancy = Arc::new(mosaica_cache::SingleFlightCache::new(
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
            // Unbounded until `set_cache_bounds` is called.
            row_projection_cache: Arc::clone(&row_projection_cache),
            fragments,
            region_cache: Arc::clone(&region_cache),
            max_region_cells: AtomicU64::new(crate::region::DEFAULT_MAX_REGION_CELLS as u64),
            artifact_projections: Arc::clone(&artifact_projections),
            shapes: Arc::clone(&shapes),
            figures: Arc::new(crate::figures::FiguresCache::new(
                u64::MAX,
                Some(cache_dir.join(crate::figures::FIGURES_DIR)),
            )),
            occupancy: Arc::clone(&occupancy),
            derived_geometry: Arc::new(crate::derived::cache::DerivedCache::default()),
            suggest_sets: Arc::new(crate::suggest_set::SuggestSets::default()),
            lineages: Arc::new(crate::cut::Lineages::new()),
            slots: Arc::new(crate::slots::SlotsCache::new()),
            level_contents: Arc::new(crate::artifact_content::LevelContents::new()),
            pool,
            count_pool,
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

        // Only the shards being served can ever read their figures and fragments back.
        let live: Vec<[u8; 32]> = engine
            .generation
            .load()
            .shards()
            .map(|shard| shard.identity)
            .collect();
        engine.figures.sweep(live.iter().copied());
        let fragments = Arc::clone(&engine.fragments);
        engine.pool.spawn(move || {
            fragments.sweep(&live);
        });

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
        // The rows the log held and no flush wrote, which the occupancy bound and the row trigger
        // count from the first request, not from the first write.
        let buffered = engine
            .generation()
            .shards()
            .map(|shard| shard.buffer.len())
            .sum();
        engine
            .write
            .health()
            .buffered_items
            .store(buffered, Ordering::SeqCst);
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
    /// postings and the shard's identity; without one those carry forward unchanged. Runs on the
    /// write executor, so there is one publisher, and blocks until the swap is done.
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
            figures: Arc::clone(&self.figures),
            fragments: Arc::clone(&self.fragments),
            region_cache: Arc::clone(&self.region_cache),
            shapes: Arc::clone(&self.shapes),
            lineages: Arc::clone(&self.lineages),
            level_contents: Arc::clone(&self.level_contents),
            suggest_dir: self.suggest_dir.clone(),
            compaction: self.config.compaction,
            switches: Arc::clone(&self.switches),
            refresh: crate::refresh::RefreshDeps {
                cache: Arc::clone(&self.row_projection_cache),
                fragments: Arc::clone(&self.fragments),
                pool: Arc::clone(&self.pool),
                in_flight: Arc::clone(&self.refresh_in_flight),
                counters: Arc::clone(&self.counters),
                switches: Arc::clone(&self.switches),
                projection_routes: Arc::clone(&self.projection_routes),
            },
            bundle_root: self.bundle_root.clone(),
            identity_key: self.identity_key,
            pool: Arc::clone(&self.pool),
        }
    }

    /// As [`Engine::start_write_executor`], with a fault switchboard armed. Test builds only.
    #[cfg(feature = "fault-injection")]
    pub fn start_write_executor_with_faults(
        &mut self,
        queue_bound: usize,
        faults: Arc<mosaica_lifecycle::faults::FaultSwitchboard>,
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
/// could not find. Opens with [`mosaica_store::open_written_prefix`], not `open_bundle`: the
/// caller wrote and digested these bytes moments ago.
pub(crate) fn open_rotation(
    bundle_root: &Path,
    prefix: &str,
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
    let prefix_dir = bundle_root.join(prefix);
    let mut bundle = mosaica_store::open_written_prefix(bundle_root, prefix)
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
    if let Some(partition) = bundle.partitions.values().next() {
        bundle.manifest =
            mosaica_store::unique::with_unique_flags(&bundle.manifest, &partition.manifest);
    }
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

    let record = &partition.only_shard().record;
    let identity = record.identity;
    let postings = open_postings(&prefix_dir, mosaica_store::ShardLayout::new(&phash, record.id))
        .map_err(|e| PublishGeometryError::PrefixNotOpenable(e.to_string()))?;

    // Opened over the new prefix, from its own manifests: cloning the live generation's would
    // serve the superseded prefix's pre-fold files, missing the blanking and the folded extents. A
    // declared column whose files are missing refuses here rather than reading as no value.
    let ShardIndexes {
        filter_columns,
        unique,
        edited,
    } = open_shard_indexes(&prefix_dir, &bundle, &phash, record, &unfolded_attributes)
        .map_err(|e| PublishGeometryError::PrefixNotOpenable(e.to_string()))?;
    let space_contents = crate::artifact_content::open_space_contents(&prefix_dir, &bundle)
        .map_err(|e| PublishGeometryError::PrefixNotOpenable(e.to_string()))?;
    let tenancy = Arc::new(
        mosaica_store::read::open_tenancy_index(
            &prefix_dir,
            record,
            mosaica_store::read::Digests {
                side: &partition.manifest.files,
                bundle: &bundle.manifest.files,
            },
        )
        .map_err(|e| PublishGeometryError::PrefixNotOpenable(e.to_string()))?,
    );

    Ok((
        bundle,
        crate::geometry::PrefixRotation {
            postings,
            identity,
            filter_columns,
            unique,
            edited,
            tenancy,
            retired,
            frees: false,
            retired_artifacts: std::collections::BTreeMap::new(),
            space_contents,
        },
    ))
}

fn read_current(root: &Path) -> Result<CurrentPointer> {
    let bytes = std::fs::read(root.join("CURRENT")).map_err(EngineError::Io)?;
    serde_json::from_slice(&bytes)
        .map_err(|e| EngineError::Malformed(format!("CURRENT is not valid JSON: {e}")))
}

/// The second half of the fragment cache's key beside the shard's identity: the rule that turns a
/// credential's terms into the set of index keys a fragment unions. A change to that rule changes
/// this text, so a cache directory kept across the change misses rather than serving a fragment
/// built under the old rule.
pub(crate) fn authorisation_rule_hash() -> [u8; 32] {
    use sha2::Digest;
    sha2::Sha256::digest(b"mosaica access expressions:1").into()
}

pub(crate) fn hex_encode(bytes: &[u8]) -> String {
    use std::fmt::Write;
    let mut s = String::with_capacity(bytes.len() * 2);
    for b in bytes {
        let _ = write!(s, "{b:02x}");
    }
    s
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

    const POOL_PANIC_CHILD: &str = "MOSAICA_POOL_PANIC_CHILD";

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
