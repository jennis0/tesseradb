//! The engine's error type.

use std::io;

use tessera_lifecycle::wal::WalError;
use tessera_plugin::PluginError;
use tessera_store::StoreError;

/// Engine-level failures. Every variant here is fail-closed: none of them hand back a partial or
/// best-effort result.
#[derive(Debug)]
pub enum EngineError {
    /// An attribute filter could not be answered because an artefact it needs could not be read.
    /// A refusal, not an empty result, which would hide an uncomputed answer from a real one.
    FilterRefused(String),
    /// The filter expression itself is malformed: an undeclared column, too deep, or a negation
    /// that does not name exactly one column. Separate from [`Self::FilterRefused`] because a
    /// column's existence and family are published schema, safe to name back unlike a failed
    /// read. An unknown value is neither of these; it is an empty operand, never an error.
    FilterMalformed(String),
    /// A browse request named a layer, level or page this deployment does not publish to this
    /// principal. Always the caller's fault: every arm names schema off `/v1/meta`, never an
    /// artefact, which returns an empty page instead.
    BrowseRefused(crate::browse::BrowseRefused),
    Store(StoreError),
    Wal(WalError),
    Plugin(PluginError),
    Io(io::Error),
    /// A viewport request named a view this bundle doesn't have.
    UnknownView(String),
    /// A view holds a segment whose rows have no known place in the view's row space: only the
    /// build segment legitimately has no extent. Defaulting one to `row_base 0` would gather
    /// points from one entity under another's identity, every count plausible, every mark wrong.
    SegmentWithoutRowBase {
        view: String,
        seg_id: String,
    },
    /// This generation's deny mask has no entry for a view its bundle carries. Every view gets an
    /// entry, empty when nothing is denied, so a missing one can never mean "nothing is denied
    /// here" — composing with the deny half absent would serve every suppressed and deleted row.
    DenyMaskMissing {
        view: String,
    },
    /// A view carried by more than one partition. `Engine::viewport` resolves a view from one
    /// partition and computes θ's anchor over it alone, which must be session-global — a
    /// per-partition anchor would serve a half-masked bundle with no error. Unreachable today:
    /// the build emits exactly one partition.
    MultiPartitionView(String),
    /// A bundle-level file (`CURRENT`, a plugin hash) was not the shape this engine expects.
    Malformed(String),
    /// `/v1/categories` was asked for a `visibility = "derived"` column whose per-`(column, code)`
    /// derived postings could not be read. Refused rather than served empty: an empty value set
    /// is what a principal who may see none of them is told, and serving it would let a blank
    /// legend read as computed.
    VocabularyVisibilityUnavailable {
        column: String,
        detail: String,
    },
    /// `GET /v1/categories/{column}/suggest` was asked for a column whose vocabulary has no
    /// suggestion index, or whose index could not be walked. Refused rather than answered empty,
    /// on the sibling variant's reasoning; `/v1/categories` itself is unaffected.
    SuggestionUnavailable {
        column: String,
        detail: String,
    },
    /// A `/v1/viewport` request's `(zoom, bbox)` spans more tiles than this engine will serve. An
    /// availability bound, not a tuning knob: the tile set is the caller-chosen product of the
    /// two, counted and refused rather than allocated and survived.
    TooManyTiles {
        demanded: u64,
        limit: usize,
    },
    /// A `/v1/viewport` request asked for an underlay this engine will not serve. Rejected, never
    /// clamped: a Morton prefix carries no depth of its own, so a silently-reduced offset would
    /// hand the client cells it cannot interpret.
    UnderlayRefused(String),
    /// This session's row projection was being built by a concurrent request, and this request
    /// waited until the wait budget ran out. A racer parks on the build rather than being
    /// refused, since refusing sheds no load while the work is already happening. Maps to 429
    /// `backpressure` with `Retry-After`.
    ProjectionBuilding,
    /// This credential's mask fragment is being built by a concurrent `authorise` call. Same
    /// non-blocking-waiters rule and 429 mapping as [`Self::ProjectionBuilding`]: this call does
    /// not wait, and the caller is told to retry rather than handed a fail-closed 500.
    FragmentBuilding,
    /// The caller's [`crate::cancel::CancelToken`] was observed flipped mid-request — a client
    /// that aborted a fetch it no longer needs. Whole-request abort: no partial `ViewportOut` is
    /// ever constructed past that point.
    Cancelled,
    /// `Engine::open` failed to build the shared `rayon::ThreadPool` from
    /// `EngineConfig::compute_threads`. Fail-closed: no fallback to per-request threading.
    ThreadPoolBuild(String),
    /// The `EngineConfig` handed to `Engine::open` switches off something a viewer relies on.
    ConfigRefused(String),
    /// `POST /v1/items/{tessera_id}`: the caller-supplied `idset` does not match the idset of the
    /// generation [`crate::viewport::Engine::item`] loaded for this call. Checked inside `item`
    /// against the same load the lookup already needs, so a generation swap landing between two
    /// separate loads cannot check one snapshot and serve another. Maps to HTTP 409 `conflict`.
    StaleIdSet,
    /// A `POST /v1/items` request the caller can correct: a field or system field it named, or
    /// one of its paging arguments. Every arm names only what the caller sent and published
    /// schema.
    RecordsRefused(crate::records::RecordsRefused),
    /// A records cursor that did not open: altered, forged, or issued to another credential,
    /// route, view, view incarnation or order. One variant with one message for every reason, so
    /// the refusal does not say which binding failed.
    CursorRefused,
}

impl std::fmt::Display for EngineError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            EngineError::FilterRefused(why) => write!(f, "filter refused: {why}"),
            EngineError::FilterMalformed(why) => write!(f, "filter refused: {why}"),
            EngineError::BrowseRefused(why) => write!(f, "browse refused: {why}"),
            EngineError::Store(e) => write!(f, "store error: {e}"),
            EngineError::Wal(e) => write!(f, "wal error: {e}"),
            EngineError::Plugin(e) => write!(f, "plugin error: {e}"),
            EngineError::Io(e) => write!(f, "io error: {e}"),
            EngineError::UnknownView(view) => write!(f, "unknown view '{view}'"),
            EngineError::SegmentWithoutRowBase { view, seg_id } => write!(
                f,
                "view '{view}' holds segment '{seg_id}', which has no extent and so no known \
                 row_base — the row space and the segment list disagree about what this view \
                 holds (see EngineError::SegmentWithoutRowBase's doc)"
            ),
            EngineError::DenyMaskMissing { view } => write!(
                f,
                "this generation's deny mask has no entry for view '{view}', so the mask and \
                 the bundle disagree about what it holds (see EngineError::DenyMaskMissing's doc)"
            ),
            EngineError::MultiPartitionView(view) => write!(
                f,
                "view '{view}' is carried by more than one partition, which this engine's \
                 single-anchor selection does not yet support (see \
                 EngineError::MultiPartitionView's doc)"
            ),
            EngineError::Malformed(detail) => write!(f, "malformed: {detail}"),
            EngineError::VocabularyVisibilityUnavailable { column, detail } => write!(
                f,
                "column '{column}' declares `visibility = \"derived\"`, and its per-viewer value \
                 visibility could not be derived ({detail}). This column's values are refused \
                 rather than published unfiltered, and rather than served empty — an empty value \
                 set is what a principal who may see none of them is told"
            ),
            EngineError::SuggestionUnavailable { column, detail } => write!(
                f,
                "column '{column}' cannot be suggested over ({detail}). The suggestion index is \
                 derived rather than built, so this refuses a typeahead and nothing else — \
                 /v1/categories over the same column is unaffected"
            ),
            EngineError::TooManyTiles { demanded, limit } => write!(
                f,
                "this (zoom, bbox) spans {demanded} tiles, above the configured limit of {limit}; \
                 narrow the bbox or request a shallower zoom"
            ),
            EngineError::UnderlayRefused(detail) => write!(f, "underlay refused: {detail}"),
            EngineError::ProjectionBuilding => write!(
                f,
                "this session's row projection is being built by a concurrent request; retry \
                 shortly"
            ),
            EngineError::FragmentBuilding => write!(
                f,
                "this credential's mask fragment is being built by a concurrent request; retry \
                 shortly"
            ),
            EngineError::Cancelled => write!(f, "request cancelled"),
            EngineError::ThreadPoolBuild(detail) => {
                write!(f, "failed to build the shared compute pool: {detail}")
            }
            EngineError::ConfigRefused(detail) => write!(f, "engine config refused: {detail}"),
            EngineError::StaleIdSet => write!(f, "stale idset"),
            EngineError::RecordsRefused(why) => write!(f, "{why}"),
            EngineError::CursorRefused => write!(
                f,
                "this cursor was not issued for this request; pass the cursor the previous \
                 response of this read returned, or start again without one"
            ),
        }
    }
}

impl std::error::Error for EngineError {}

pub type Result<T> = std::result::Result<T, EngineError>;
