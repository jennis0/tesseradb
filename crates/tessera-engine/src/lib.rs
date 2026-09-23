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
mod bundle_lock;
mod cache;
pub mod cancel;
mod categories;
mod coalesce;
mod compact;
mod config;
pub mod compose;
pub mod containment;
mod control;
pub mod cut;
pub mod derived;
mod engine;
mod error;
pub mod filter;
mod flush;
pub mod gate;
mod generation;
mod geometry;
pub mod histogram;
pub mod layout;
pub mod membership_column;
mod merge;
pub mod occupancy;
pub mod projection;
pub mod records;
mod refresh;
pub mod region;
pub mod row_column;
pub mod select;
pub mod session;
pub mod shapes;
mod stage;
mod status;
pub mod suggest;
pub mod suggest_set;
mod switches;
mod test_hooks;
pub mod tile_index;
pub mod timing;
pub mod view_declarations;
pub mod viewport;
mod vocabularies;
mod write;


use arc_swap::ArcSwap;


pub use cancel::CancelToken;
pub use categories::{
    CategoryColumn, CategoryPage, CategoryQuery, CategoryValue, MatchSpan, SuggestPage, Suggestion,
};
// The fold's automatic trigger, as a value an operator's configuration builds. `tessera-server`
// parses `ingest.compaction_*` into one of these and hands it over in `EngineConfig`; the executor
// is the only reader. The rest of `compact` stays private — what a fold *is* is this crate's
// business, and when it runs is the deployment's.
pub use compact::{CompactionSchedule, PassCost};
pub use compose::{buffered_rows_of, compose, denied_rows_of, visible_to, EffectiveMask};
pub use projection::{ProjectionInputs, ProjectionRoute, RowProjection};
pub use records::{
    ItemsCounts, ItemsHead, ItemsLimits, ItemsPageEnd, ItemsRequest, ItemsSink, ItemsTrailer,
    PageEndedBy, RecordsOrder, RecordsRefused, ResponseEndedBy,
};
// The publication guard's refusal, which a publisher outside this crate must handle.
// `check_publishable` itself stays private: whether a geometry may be published is this crate's
// judgement, and a caller that could ask separately could also act on a stale answer.
pub use gate::VisibleViews;
pub use geometry::{GeometryPublication, GeometryRefused, GeometryRefusedReason};
pub use config::{default_compute_threads, EngineConfig, MIN_K_MIN, MIN_SELECTION_WIDTH};
pub use control::{GrownMembership, PublishedArtifacts};
pub use engine::Engine;
pub use error::EngineError;
pub use session::Session;
pub use status::{GenerationStatus, PartitionStatus, ViewSegments};
pub use tessera_cache::{CacheStats, DEFAULT_WAIT_BUDGET_MS as DEFAULT_SINGLE_FLIGHT_WAIT_MS};
pub use derived::ComputedProperty;
pub use membership_column::MembershipColumn;
pub use region::{RegionRows, RegionVerdict, DEFAULT_MAX_REGION_CELLS};
// The fragment tier's gauges. Renamed because they are the same type as `CacheStats` above, and a
// bare second `CacheStats` in one namespace would be a coin toss at every call site.
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
// A batch's layer column is read by the rule a build reads a member table's key column by.
pub use tessera_store::member_key;
// A batch's scalar column is read by the rule a build reads a points file's attribute column by,
// and each row's value arrives as the build's value type.
pub use tessera_spatial::tiler::ScalarValue;
pub use tessera_store::scalar_column;
// A batch's category column is read by the rule a build reads a points file's category keys by.
pub use tessera_store::utf8;
// The write path's **outcome** vocabulary, and nothing else.
//
// `LifecycleHandle`, `LifecycleQueues`, `Command` and `Reply` are deliberately **not** here, and
// the tempting design where a handler holds the handle and submits through it is refused:
// `LifecycleHandle` is not `Clone` and `WritePath` is its sole owner, precisely so `WritePath::drop`
// can disconnect the queues and join the thread. No handler can hold one, and none of those four
// types appears anywhere outside the `write` module.
//
// What a handler does hold is `Engine`, and it submits through `Engine::accept_ingest` /
// `Engine::accept_change` — blocking calls, hence inside `spawn_blocking`. What it needs from here
// is how to answer: `AcceptError` for the status mapping, and `ExecutorPosture`/`ExecutorStats`
// for `readyz` and `/control/status`.
pub use bundle_lock::BundleLockError;
pub use write::{
    AcceptError, ExecutorHealth, ExecutorPosture, ExecutorStartError, ExecutorStats, FoldRefusal,
    PendingChange, PublishGeometryError, ValuesReceipt, WalGauge, WriteStage,
    DENY_DURABILITY_ATTEMPTS, DENY_WINDOW_MAX_ENTRIES, FOLD_GATES,
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

pub use generation::{Generation, GenerationParts};

/// Per-view row-space deny masks — see [`Generation::denied`].
pub type DenyMask = rustc_hash::FxHashMap<String, croaring::Bitmap>;

/// Per view, the buffered entities that already have a row there — see
/// [`Generation::buffered_rows`].
pub type BufferedRows = rustc_hash::FxHashMap<String, Vec<tessera_types::EntityId>>;

/// The process-wide handle to the current generation. A request must load this pointer exactly
/// **once**, at request start, before acquiring any fragment or cache entry — loading it more
/// than once within a single request risks composing a fragment built against one generation's
/// bundle/watermark against an overlay or buffer swapped in from a later one, which is exactly
/// the kind of cross-generation mismatch `RowProjection`'s cache key and `FrozenFragment`'s
/// stored watermark both assume cannot happen. It is also the whole of I11's within-request rule
/// — see `crate::geometry`.
pub type GenerationHandle = ArcSwap<Generation>;
