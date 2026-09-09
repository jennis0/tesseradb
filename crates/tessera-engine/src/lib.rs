//! `tessera-engine` — the request-serving core: generation snapshots and the I1 mask composition.
//!
//! [`Generation`] is one immutable snapshot of everything a request needs: the loaded bundle, the
//! live overlay and ingest buffer, and the version counters that identify it. A running process
//! holds the current generation behind an `arc_swap::ArcSwap`, atomically swapped whenever a
//! `/control/changes` or `/control/ingest` acceptance advances the overlay/buffer, or a
//! `tessera build` advances the bundle.

pub mod artifact_content;
pub mod artifacts;
mod attributes;
pub mod browse;
mod cache;
pub mod cancel;
mod categories;
mod coalesce;
mod compact;
pub mod compose;
pub mod containment;
pub mod cut;
pub mod derived;
pub mod derived_cache;
pub mod filter;
mod flush;
pub mod gate;
mod geometry;
pub mod histogram;
pub mod layout;
pub mod membership_column;
mod merge;
pub mod occupancy;
mod refresh;
pub mod region;
pub mod row_column;
pub mod select;
pub mod session;
pub mod shapes;
mod single_flight;
mod stage;
pub mod suggest;
pub mod suggest_set;
pub mod tile_index;
pub mod timing;
pub mod view_declarations;
pub mod viewport;
mod vocabularies;
mod write;

use std::sync::Arc;

use arc_swap::ArcSwap;

use tessera_authz::{DeltaTier, Dict, FragmentCache, PostingsReader};
use tessera_lifecycle::{IngestBuffer, Overlay};
use tessera_store::Bundle;

pub use cancel::CancelToken;
pub use categories::{
    CategoryColumn, CategoryPage, CategoryQuery, CategoryValue, MatchSpan, SuggestPage, Suggestion,
};
// The fold's automatic trigger, as a value an operator's configuration builds. `tessera-server`
// parses `ingest.compaction_*` into one of these and hands it over in `EngineConfig`; the executor
// is the only reader. The rest of `compact` stays private — what a fold *is* is this crate's
// business, and when it runs is the deployment's.
pub use compact::{CompactionSchedule, PassCost};
pub use compose::{compose, denied_rows_of, visible_to, EffectiveMask, RowProjection};
// The publication guard's refusal, which a publisher outside this crate must handle.
// `check_publishable` itself stays private: whether a geometry may be published is this crate's
// judgement, and a caller that could ask separately could also act on a stale answer.
pub use gate::VisibleViews;
pub use geometry::{GeometryPublication, GeometryRefused, GeometryRefusedReason};
pub use session::{
    default_compute_threads, Engine, EngineConfig, EngineError, GrownMembership, PartitionStatus,
    PublishedArtifacts, Session, ViewSegments,
};
// The row-projection cache's gauges. `single_flight` itself stays private — the cache, its slot
// state machine and its four eviction rules are engine-internal — but the numbers
// `/control/status` publishes have to cross the crate boundary.
pub use single_flight::{CacheStats, DEFAULT_WAIT_BUDGET_MS as DEFAULT_SINGLE_FLIGHT_WAIT_MS};
// The fragment tier's gauges, under a distinguishing name because the two are the same shape and a
// bare second `CacheStats` in one namespace would be a coin toss at every call site.
//
// **This re-export is what makes `Engine::fragment_cache_stats`'s return type nameable at all.**
// `tessera-server` may not depend on `tessera-authz` (SA §3; `scripts/check-layers.sh` has
// `deny tessera-server tessera-authz`), and `tessera_authz::CacheStats` is not a public path even
// for a crate that could — its module is private there. Whoever wires `/control/status`
// writes `use tessera_engine::FragmentCacheStats;` and nothing else.
pub use derived::ComputedProperty;
pub use membership_column::MembershipColumn;
pub use region::{RegionRows, RegionVerdict, DEFAULT_MAX_REGION_CELLS};
pub use tessera_authz::fragment::CacheStats as FragmentCacheStats;
pub use timing::{Probe, StageTimings};
pub use viewport::{
    ArtifactOut, ArtifactRows, ColumnBuf, ComputedSelection, EngineMeta, ItemOut, LayerSelection,
    LeafColumn, LevelSelection, MetaGroup, MetaRoster, MetaView, PointColumns, PointRows,
    ScalarOut, SinkClosed, SinkResult, SubCellCount, TileAddress, TileCount, ViewCoordinates,
    ViewportHead, ViewportOut, ViewportRequest, ViewportSink,
};
// `EngineMeta::declared_scalars`' element type, re-exported for the same layering reason
// `FragmentCacheStats` is: `check-layers.sh` denies a `tessera-server → tessera-store` edge
// (SA §3), and `/control/ingest` validates a batch's scalar tail against this declaration, so the
// type needs to be nameable from the crate that reads it. `/v1/meta` gets away without naming it
// only because it reads the two fields straight into JSON.
pub use tessera_store::manifest::DeclaredScalar;
// The ingest handler resolves category keys to codes and must name the reserved *absent* code and
// the binding view to do it. Re-exported for the same layering reason as `DeclaredScalar`.
pub use tessera_store::manifest::{
    ManifestVocabulary, ManifestVocabularyValue, Visibility, VocabularyKind,
};
pub use tessera_store::vocabulary::{Vocabularies, VocabularyMinter, ABSENT_CODE};
// `MetaRoster::metadata`'s value type. `/v1/meta` publishes a view's roster metadata typed
// (`views.md` §3.2), so the server has to name the variants to write the wire's `type` tag —
// re-exported for the same layering reason `DeclaredScalar` is.
pub use tessera_store::manifest::ViewMetadataValue;
// `EngineMeta::scoped_scalars`' element type. `/v1/meta` publishes a scoped family's operand entry
// with the group it is scoped to (`views.md` §5), so the server has to name it — re-exported for
// the same layering reason `DeclaredScalar` is.
pub use tessera_store::manifest::ScopedScalar;
// `DeclaredScalar::arrow_type`'s type, and `wire_type`'s. The server names it to widen a code to
// its column's storage width, and reaches it here rather than transcribing the table again.
pub use tessera_spatial::tiler::ScalarType;
// `MetaView::projection`'s type. `/control/ingest` reads it to decide what a batch's coordinate
// columns are called and what the numbers in them mean (`projections.md` §3), so the type has to
// be nameable from the crate that decodes the batch — the same reason `ScalarType` is here.
pub use tessera_spatial::Projection;
// `EngineMeta::quantisation`'s type, re-exported for the same layering reason `DeclaredScalar` is:
// `check-layers.sh` denies a `tessera-server → tessera-store` edge (SA §3), and `/control/ingest`
// validates an ingested coordinate against this declaration (§6), so the type needs to be nameable
// from the crate that reads it.
pub use tessera_store::manifest::Quantisation;
// The write path's **outcome** vocabulary, and nothing else.
//
// `LifecycleHandle`, `LifecycleQueues`, `Job` and `Responder` are deliberately **not** here, and
// the tempting design where a handler holds the handle and submits through it is refused:
// `LifecycleHandle` is not `Clone` and `WritePath` is its sole owner, precisely so `WritePath::drop`
// can disconnect the queues and join the thread. No handler can hold one, and none of those four
// types appears anywhere outside `write.rs`.
//
// What a handler does hold is `Engine`, and it submits through `Engine::accept_ingest` /
// `Engine::accept_change` — blocking calls, hence inside `spawn_blocking`. What it needs from here
// is how to answer: `AcceptError` for the status mapping, and `ExecutorPosture`/`ExecutorStats`
// for `readyz` and `/control/status`.
pub use write::{
    AcceptError, ExecutorHealth, ExecutorPosture, ExecutorStartError, ExecutorStats, PendingChange,
    PublishGeometryError, ValuesReceipt, WriteStage, DENY_DURABILITY_ATTEMPTS,
    DENY_WINDOW_MAX_ENTRIES,
};
// The flush's own laps, beside `WriteStage`'s and read by the same status block.
pub use flush::FlushStage;
// The queue's own `retry_after_s` derivation. Exported because `tessera-server` derives a
// *second* 429 subject's value from the same estimator over a different depth (contracts §0.3
// deviation 11: the value is per-subject), and two independent implementations of one estimator is
// how the two subjects come to disagree about the same queue.
//
// **The floor and ceiling are exported for readers, not for callers.** `tessera-server` does not
// derive from them — it calls `estimate_retry_after_s` and inherits both. They stay `pub` because
// `estimate_retry_after_s`'s
// own doc names them as the bounds on its result, and a documented bound whose value is unreachable
// from the crate that reads the doc is a dead reference.
pub use write::{
    estimate_buffer_retry_after_s, estimate_retry_after_s, RETRY_AFTER_MAX_SECS,
    RETRY_AFTER_MIN_SECS,
};
// `PUT /control/attributes`' body as the executor resolves it, re-exported so the server sees
// engine API types only (SA §3).
pub use tessera_lifecycle::AttributeRequest;
// `POST /control/values`' body as the executor takes it, re-exported on `AttributeRequest`'s rule.
pub use tessera_lifecycle::{IncomingValues, ValuesRequest};
// The two vocabulary routes' bodies, and the two view declarations', on the same rule.
pub use tessera_lifecycle::wal::{DeclaredFrame, PlainViewDeclaration, ViewGroupDeclaration};
pub use tessera_lifecycle::{DeclaredValue, VocabularyRequest};

/// One immutable, atomically-swappable snapshot of engine state (lifecycle §1.1).
///
/// **⊘ No compaction exists**, but this type is what one would publish: the fields a fold rotates
/// — the base postings, the fragment cache and the bundle identity it keys, and the external-id
/// sidecar — are here rather than on `Engine`, which is what makes a prefix flip expressible at all
/// (compaction §4). Merge publishes through this type too: the entity-space coalesce without moving
/// `segments_version`, the row-space merge as its own swap (`crate::coalesce`, `crate::merge`).
pub struct Generation {
    /// The bundle's `CURRENT` prefix (e.g. `"v00000"`) this generation was loaded from.
    pub prefix: String,
    /// Monotone counter identifying this generation's segment set — bumped only when a new
    /// bundle build is loaded, never by an overlay/buffer update.
    pub segments_version: u64,
    /// The SEGMENTS manifest's watermark: the highest entity id folded into the bundle's row
    /// geometry. Entities at or past this value live only in `buffer`, never in `bundle`'s
    /// permutations (I1 composition rule 4).
    pub watermark: u64,
    pub bundle: Arc<Bundle>,
    /// The dictionary this generation's postings and buffered items resolve against.
    ///
    /// **Generation-scoped rather than process-scoped, because a flush promotes.** A novel
    /// descriptor buffers an item under an unsatisfiable extension id and becomes a durable
    /// ordinal only when the flush that carries it publishes a `dict_extents` entry (§3.2), so
    /// the dictionary grows with geometry and has to be republished alongside it. Ordinals are
    /// preserved across a promotion ([`tessera_authz::Dict::load_extending`]), so a session
    /// authorised against an older generation keeps evaluating the terms it was granted; what it
    /// does *not* get is the newly promoted one, which is fail-closed and is what §3.3's
    /// staleness hint exists to advertise.
    pub dict: Arc<Dict>,
    /// The base postings — the build's `terms/postings.arrow`, unchanged by any flush.
    pub postings: Arc<PostingsReader>,
    /// The mask-fragment cache, and through it **this generation's bundle identity** — the
    /// MANIFEST digest of the prefix `postings` was read from.
    ///
    /// **On the generation rather than beside it, and that is what makes Rule F's safety
    /// structural** (write-path §5.4, compaction §4). A fold rewrites the term index and publishes
    /// a new prefix, so every fragment built from the old one names entities the new postings no
    /// longer contain — and it advances no watermark, so nothing keyed on the watermark can see
    /// it. Bound at `Engine::open` for the process lifetime, as it was, nothing could rotate the
    /// fragment identity in-process at all, and a fold published through that seam would leave
    /// every pre-fold fragment reachable by key — *including* the persisted `.frag` files, across
    /// a restart. Here, a request loads one pointer and gets postings, identity and fragment cache
    /// that agree, exactly as I11's within-request rule already requires for geometry.
    ///
    /// **Swapping this is necessary and is not sufficient.** Two holders sit outside it — a
    /// `Session`'s own `Arc<FrozenFragment>`, and `SessionGeometry`'s — and a replaced container
    /// reaches neither. The comparison is therefore made at composition, against
    /// [`tessera_authz::FrozenFragment::identity`]: `Engine::fragment_for` for the first,
    /// `RowProjectionCache::freshest_fragment`'s prefix scoping for the second.
    pub(crate) fragments: Arc<FragmentCache>,
    /// External ids established by the bundle's runs — the reader half of contracts §2.4.
    ///
    /// **Per generation, because a fold is not content-preserving.** The entity-space coalesce
    /// that first made this swappable is: an old sidecar and a new generation answer identically
    /// for every key, so which one a request held could not be observed. A fold drops the retired
    /// entities' keys (compaction §3, pass 3) and rewrites the locator into a new prefix, so a
    /// request pairing the new geometry with the pre-fold sidecar would resolve through files the
    /// old prefix holds and reclamation is about to delete. One pointer, one answer.
    pub(crate) external_index: Arc<crate::session::ExternalIdIndex>,
    /// One sparse delta postings tier per flush segment, in publication order.
    ///
    /// A fragment build unions the base with every live tier over the session's satisfied terms
    /// (§5.2). They live on the generation rather than on the engine for the same reason the
    /// dictionary does: a flush publishes one, and a merge coalesces several into one, so the set
    /// changes exactly when geometry does. Empty in a bundle straight out of `tessera build`.
    pub delta_postings: Vec<Arc<DeltaTier>>,
    /// Monotone counter bumped on every overlay/buffer swap (independent of `segments_version` —
    /// an overlay change never touches the bundle).
    pub overlay_version: u64,
    pub overlay: Arc<Overlay>,
    pub buffer: Arc<IngestBuffer>,
    /// The live category bindings: key → code per vocabulary, plus the assigned-code set that makes
    /// never-reuse hold (per-point-attributes §3.4).
    ///
    /// **On the generation, because a mint publishes.** A novel key acquires its code at a commit
    /// window's close and becomes durable in the same fsync as the rows that use it, so the
    /// bindings grow exactly when the buffer does and have to be republished alongside it — the
    /// same reason `dict` lives here. A request that loads one generation pointer gets the buffer,
    /// the geometry and the bindings that agree.
    ///
    /// **Read-only here; the executor owns the authoritative copy.** Minting is serial by
    /// construction (write-path §1.1) and this is a published snapshot of it, exactly as `overlay`
    /// is of the live overlay. A handler resolving a key through this may find it bound or not; it
    /// must never mint, because two handlers racing one novel key would draw two codes for it and
    /// split its rows between them.
    pub vocabularies: Arc<Vocabularies>,
    /// **The deny mask**: per view, the row-space image of `deleted ∪ suppressed`, subtracted
    /// from every composed mask (I1).
    ///
    /// **Derived, never persisted, never a second source of truth.** The three entity-space stores
    /// on [`Overlay`] remain authoritative, and `compose::verdict` remains the single answer for
    /// every entity-space verb — `visible_to`, label gating, cluster visibility. This exists
    /// because the *row-space* question was being answered by walking the deny sets and resolving
    /// `row_of` per denied entity on every request, which made per-request work grow with denies
    /// **ever accepted**. Folded in as a bitmap, the deny half of composition costs one `andnot`.
    ///
    /// **It cannot go stale, because it never outlives its generation.** Row ids mean something
    /// only within one `segments_version`, so the mask is rebuilt by every geometry publication and
    /// travels with the row space it addresses — a request that loads one generation pointer gets
    /// the overlay, the buffer and the mask that agree.
    ///
    /// **The derivation rule is in [`crate::compose::derive_denied`]**, and the trap it names —
    /// that an unsuppress may not subtract a row — is the one way this could silently re-expose a
    /// deleted item. `publish` re-derives in debug and asserts equality, so a build site that gets
    /// it wrong fails in the test suite rather than in a viewer's map.
    ///
    /// Keyed by view, because row space is. A view the bundle carries always has an entry, empty
    /// when nothing is denied; a missing entry means the mask and the bundle disagree about what
    /// this generation holds, and the read path treats that as fail-closed rather than as "nothing
    /// denied".
    pub denied: Arc<DenyMask>,
    /// The bundle's filter columns, opened once per generation.
    ///
    /// **Generation-scoped for the same reason the dictionary is**: the artefact belongs to the
    /// published prefix, so a new bundle brings new columns and a session reading the old
    /// generation keeps reading the old ones. Empty when the schema declares nothing filterable.
    pub filter_columns: Arc<crate::filter::FilterColumns>,
    /// Every category vocabulary's suggestion index (`value-suggestion.md` §6.1).
    ///
    /// **Carried across publications rather than rebuilt with them.** A flush changes which
    /// entities carry a value and changes nothing this index holds — it is over the value set, and
    /// the mask never enters it — so a per-generation rebuild would pay a sort measured in tens of
    /// seconds at 10⁷ values for a change it cannot see. It is on the generation all the same,
    /// because a *mint* publishes: a novel key acquires its code at a commit window's close and
    /// must be suggestible on the next keystroke, so the side map grows exactly when
    /// [`Generation::vocabularies`] does and travels with it.
    pub suggest: Arc<crate::suggest::SuggestIndexes>,
}

impl Generation {
    /// The MANIFEST digest of the prefix this generation's postings, dictionary and term index
    /// were read from — see [`Generation::fragments`], which is where it is held so that it and
    /// the cache it keys cannot disagree.
    pub(crate) fn bundle_identity(&self) -> [u8; 32] {
        self.fragments.bundle_identity()
    }

    /// What a containment partition is composed from, for this generation — the base postings and
    /// the manifest's declared plugin, taken together so the gate cannot be applied to one
    /// generation's postings on another generation's manifest
    /// (see [`crate::containment::PartitionSource`]).
    pub(crate) fn partition_source(&self) -> crate::containment::PartitionSource<'_> {
        crate::containment::PartitionSource {
            postings: &self.postings,
            data_plugin_hash: &self.bundle.manifest.data_plugin_hash,
        }
    }
}

/// The two [`Generation`] fields a synthetic fixture cannot meaningfully build, for the three
/// in-crate test modules that construct a `Generation` over an *empty* bundle.
///
/// The fragment cache is over a path nothing writes — [`FragmentCache::new`] touches no filesystem,
/// and a fixture whose bundle carries no postings never reaches a build. The sidecar is over a
/// manifest naming no runs, which is the same `deferred` shape a bundle with no external ids opens
/// with. Both exist so that adding a field to `Generation` stays a compile error at the sites that
/// have an opinion about it, and one line at the sites that do not.
#[cfg(test)]
pub(crate) fn synthetic_generation_parts() -> (Arc<FragmentCache>, Arc<session::ExternalIdIndex>) {
    use tessera_plugin::Plugin;
    let manifest = tessera_store::manifest::SegmentsManifest {
        watermark: 0,
        entity_id_high_water: 0,
        entity_id_low_water: tessera_types::layer::ROWLESS_CEILING,
        layers: Vec::new(),
        layer_tombstones: Vec::new(),
        views: Vec::new(),
        scoped_columns: Vec::new(),
        attributes: Vec::new(),
        scoped_attributes: Vec::new(),
        vocabularies: Vec::new(),
        groups: Vec::new(),
        plain_views: Vec::new(),
        dead_view_incarnations: Vec::new(),
        membership_extents: Vec::new(),
        level_versions: Vec::new(),
        containment_extents: Vec::new(),
        tile_index_extents: Vec::new(),
        row_column_extents: Vec::new(),
        shape_rows_extents: Vec::new(),
        shape_held_extents: Vec::new(),
        artifact_record_extents: Vec::new(),
        segments: Vec::new(),
        deltas: Vec::new(),
        dict_extents: Vec::new(),
        attr_extents: Vec::new(),
        record_extents: Vec::new(),
        entity_terms_extents: Vec::new(),
        text_extents: Vec::new(),
        external_id_runs: Vec::new(),
        locator_extents: Vec::new(),
        tombstones: Vec::new(),
        deny: Vec::new(),
        vocabulary_extensions: Vec::new(),
        files: std::collections::BTreeMap::new(),
    };
    let bundle_manifest = tessera_store::manifest::Manifest {
        bundle_format: 3,
        created_at: String::new(),
        data_plugin_hash: tessera_plugin::Passthrough::new().data_plugin_hash(),
        declared_bounds: serde_json::json!({}),
        declared_scalars: vec![],
        vocabularies: vec![],
        small_term_threshold: 32,
        entity_id_high_water: 0,
        identity: tessera_store::manifest::IdentityDescriptor {
            construction: "siphash-2-4".to_string(),
            rounds: 1,
            key: "0123456789abcdef0123456789abcdef".to_string(),
            shard_id: 0,
            idset: 1,
        },
        groups: Vec::new(),
        views: vec![],
        partitions: vec![],
        provenance: serde_json::json!({}),
        files: std::collections::BTreeMap::new(),
    };
    (
        Arc::new(FragmentCache::new(
            std::path::Path::new("fixture-fragment-cache-never-written"),
            [0u8; 32],
            [0u8; 32],
        )),
        Arc::new(
            session::ExternalIdIndex::open(
                &bundle_manifest,
                &manifest,
                std::path::Path::new("fixture-prefix-never-read"),
            )
            .expect("a manifest naming no runs opens deferred"),
        ),
    )
}

/// Per-view row-space deny masks — see [`Generation::denied`].
pub type DenyMask = rustc_hash::FxHashMap<String, croaring::Bitmap>;

/// The process-wide handle to the current generation. A request must load this pointer exactly
/// **once**, at request start, before acquiring any fragment or cache entry — loading it more
/// than once within a single request risks composing a fragment built against one generation's
/// bundle/watermark against an overlay or buffer swapped in from a later one, which is exactly
/// the kind of cross-generation mismatch `RowProjection`'s cache key and `FrozenFragment`'s
/// stored watermark both assume cannot happen. It is also the whole of I11's within-request rule
/// — see `crate::geometry`.
pub type GenerationHandle = ArcSwap<Generation>;
