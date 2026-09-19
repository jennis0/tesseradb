//! The engine's error type.

use std::io;

use tessera_lifecycle::wal::WalError;
use tessera_plugin::PluginError;
use tessera_store::StoreError;

/// Engine-level failures. Every variant here is fail-closed (Global Constraint 3): none of them
/// hand back a partial or best-effort result.
#[derive(Debug)]
pub enum EngineError {
    /// An attribute filter could not be answered because an artefact it needs could not be read
    /// (`filter::FilterError`, the half `is_callers_fault` calls the deployment's).
    ///
    /// **A refusal, never an empty result.** An empty answer is a real one — it is what a principal
    /// who can see no matching item is given — so serving it for a filter that could not be
    /// computed would make an underived answer indistinguishable from a derived one.
    FilterRefused(String),
    /// The filter **expression** is malformed: an undeclared column, too deep, or a negation that
    /// does not name exactly one column (`filter::FilterError`, the caller's half).
    ///
    /// **Separate from [`Self::FilterRefused`] because the two are different status codes**, and
    /// flattening them into one string variant made every caller error a fail-closed `500`. What
    /// a caller may be told is decided by whether the fact is deployment schema — a column's
    /// existence and family are published to every principal alike — so naming them back discloses
    /// nothing. An unknown *value* is neither of these: it is an empty operand, never an error.
    FilterMalformed(String),
    /// A browse request named something this deployment does not publish to this principal — a
    /// layer, a level, or a zero-length page (`crate::browse::BrowseRefused`). **Always the
    /// caller's fault and always a `422`**: every arm names deployment schema the caller reads off
    /// `/v1/meta`, and no arm is ever about an *artifact*, which is the empty page instead.
    BrowseRefused(crate::browse::BrowseRefused),
    Store(StoreError),
    Wal(WalError),
    Plugin(PluginError),
    Io(io::Error),
    /// A viewport request named a view this bundle doesn't have.
    UnknownView(String),
    /// A view holding a segment whose rows have no known place in the view's row space.
    ///
    /// `tile_ranges` returns **segment-local** row indices (contracts §2.4) while the mask is a
    /// bitmap over the whole **view** row space, so serving a segment requires knowing its
    /// `row_base`. Exactly one segment — the build segment, the one `permutation.bin` addresses —
    /// legitimately has no extent and begins at 0; every other arrives with one, from a flush or
    /// from a merge. A second segment with no extent means the row space and the segment list
    /// disagree about what the view holds.
    ///
    /// **Fails closed because the wrong answer is quiet.** Defaulting such a segment to `row_base
    /// 0` would count its rows against the base segment's mask positions and gather points from
    /// one entity under another's identity — every count plausible, every mark wrong, no error
    /// anywhere. That is a worse outcome than a 500.
    SegmentWithoutRowBase {
        view: String,
        seg_id: String,
    },
    /// This generation's deny mask has no entry for a view its bundle carries.
    ///
    /// **Fails closed for the same reason [`Self::SegmentWithoutRowBase`] does: the wrong answer
    /// is silent.** `compose::derive_denied` gives every view an entry, empty when nothing is
    /// denied, precisely so that a missing one cannot be read as "nothing is denied here". Reading
    /// it that way would compose a mask with the deny half simply absent — every suppressed and
    /// deleted row served on the map, every count including them, and no error anywhere. A 500 is
    /// the better outcome.
    ///
    /// Unreachable while the mask and the bundle are built together, which `Executor::publish`
    /// asserts in debug.
    DenyMaskMissing {
        view: String,
    },
    /// A view carried by more than one partition.
    ///
    /// The symmetric case to [`Self::SegmentWithoutRowBase`], and it fails closed for the symmetric
    /// reason: `Engine::viewport` resolves a view by taking the first partition that carries the
    /// id, and θ's anchor plus every rank is then computed over **that partition alone**. Design
    /// §12.3 requires the anchor to be session-global across partitions — a per-partition anchor
    /// makes "below the cut" mean different things in different partitions, so the coordinator's
    /// union stops computing §7.2's definition. The build emits exactly one partition, so this
    /// is unreachable today; serving a §12 bundle half-masked with no error is what it prevents.
    MultiPartitionView(String),
    /// A bundle-level file (`CURRENT`, a plugin hash) was not the shape this engine expects.
    Malformed(String),
    /// `/v1/categories` was asked for a `visibility = "derived"` column whose per-`(column, code)`
    /// membership sets could not be read — they are the column's derived postings, and either the
    /// bundle carries none for it or the file failed to read.
    ///
    /// **Refused rather than served empty**, which is the fail-closed choice that is also the
    /// honest one. An empty value set is a real answer — it is what a principal who may see none
    /// of these values is told — so returning it here would make an underivable predicate
    /// indistinguishable from a correctly-applied one, and a viewer would render a blank legend as
    /// though it had been computed. Serving the set *unfiltered* is the other direction and is the
    /// C11 disclosure itself.
    VocabularyVisibilityUnavailable {
        column: String,
        detail: String,
    },
    /// `GET /v1/categories/{column}/suggest` was asked for a column whose vocabulary has no
    /// suggestion index, or whose index could not be walked.
    ///
    /// **Refused rather than answered empty**, on the sibling variant's reasoning exactly: an empty
    /// suggestion list is a real answer — it is what a viewer who may see nothing under their
    /// prefix is told — so serving it for a missing index makes a broken surface indistinguishable
    /// from a working one, and a client would read "no such value" where the truth is "not asked".
    /// It is not a disclosure refusal: the enumeration over the same column is unaffected, and this
    /// costs a typeahead rather than a value list.
    SuggestionUnavailable {
        column: String,
        detail: String,
    },
    /// A `/v1/viewport` request's `(zoom, bbox)` spans more tiles than this engine will serve.
    ///
    /// **This is an availability bound on the base path, not a tuning knob.** `zoom` and `bbox` are
    /// both caller-chosen and the tile set is their product, so at zoom 16 over the full extent it
    /// is 4.29e9 tiles — ~69 GB of `Vec` before any masking work. Counted and refused rather than
    /// allocated and survived.
    TooManyTiles {
        demanded: u64,
        limit: usize,
    },
    /// A `/v1/viewport` request asked for a §3.3 underlay this engine will not serve.
    ///
    /// **Rejected, never clamped** — and that is one rule for all three bounds (config offset, the
    /// depth-16 grid limit, and the total cell budget), deliberately. A Morton prefix carries no
    /// depth of its own, so a silently-reduced offset would hand the client cells it cannot
    /// interpret; rejecting means the depth is always `zoom + offset` from the caller's own request.
    UnderlayRefused(String),
    /// This session's row projection for `(token_id, view, segments_version)` was being built by
    /// a concurrent request, and **this request waited for it and the wait budget ran out**
    /// (decision 0058). It is no longer the immediate answer to finding a build in flight: a racer
    /// parks on that build and is served its result, because refusing sheds no load — the work is
    /// already happening — while the client's retry budget is shorter than the build.
    ///
    /// So this now means one of two things, and both are real: the build is taking longer than
    /// `serve.single_flight_wait_ms`, or builders are dying without publishing often enough to
    /// exhaust the budget. Decision 0044's merge-window shed also produces it, from a different
    /// rung of `Engine::session_geometry`'s ladder and for a different reason.
    ///
    /// Maps to **429 `backpressure` with `Retry-After`** at the server boundary
    /// (`tessera-server::error::map_engine_error`'s explicit arm, pinned by
    /// `map_engine_error_takes_projection_building_to_backpressure`): retryable rather than
    /// fail-closed, because a retry after this genuinely may find the value.
    ProjectionBuilding,
    /// This credential's mask fragment (lifecycle §3.3), keyed by the canonical `(bundle_identity, auth_plugin_hash, satisfied terms)` key, never
    /// `auth_data_hash` — see `tessera_authz::FragmentCache::get_or_build`'s doc — is being built
    /// by a concurrent `authorise` call right now. Same non-blocking-waiters rule and the same
    /// **429** mapping as [`Self::ProjectionBuilding`]: this call does not wait, and the caller is
    /// told to retry rather than handed a fail-closed 500.
    FragmentBuilding,
    /// D-C: the caller's [`crate::cancel::CancelToken`] was observed flipped mid-request (the
    /// rapid-pan case — a client aborted a fetch it no longer needs). Whole-request abort:
    /// [`crate::viewport::Engine::viewport`] returns this the instant a check catches the flip,
    /// and no partial `ViewportOut` is ever constructed past that point (I13a — cancelled is not
    /// an empty-but-valid contribution, it is no contribution). Maps to a fixed fail-closed 500 at
    /// the server boundary (`tessera-server::error::map_engine_error`'s explicit arm) — this must
    /// never become a 2xx or any 4xx, even if a future refactor makes the arm reachable on a
    /// still-live connection (today it is not: the server's drop-guard only flips the token when
    /// the whole handler future is dropped, which also means nobody is left to read a response).
    Cancelled,
    /// D-D: `Engine::open` failed to build the shared `rayon::ThreadPool` from
    /// `EngineConfig::compute_threads` (e.g. a platform that refuses the requested thread count).
    /// Fail-closed: an engine that cannot build its compute pool does not open at all — there is
    /// no fallback to per-request ad hoc threading or to a serial tile loop, because either would
    /// be a silent behaviour change the D-D design (one shared pool, no second throttle) does not
    /// admit.
    ThreadPoolBuild(String),
    /// `POST /v1/items/{tessera_id}` (contracts §2.2/§3.2 r6): the caller-supplied `idset` does
    /// not match the idset of the generation [`crate::viewport::Engine::item`] loaded
    /// for this call. Named explicitly so the idset check can run *inside* `item`, against the
    /// SAME `generation.load_full()` the lookup that follows already needs — not a separate
    /// `Engine::meta()` call (and its own, second `load_full`) ahead of it. That used to be two
    /// independent loads for one logical request, against lifecycle §1.1's one-load-per-request
    /// invariant: a generation swap landing between them could check the idset against one
    /// snapshot and serve the lookup from another. Maps to HTTP 409 `conflict` with a fixed
    /// detail string (`tessera-server::error::map_engine_error`'s explicit arm) — entity
    /// independent, decided before the id is inverted, so it opens no timing channel (Appendix C,
    /// C4).
    StaleIdSet,
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
            EngineError::StaleIdSet => write!(f, "stale idset"),
        }
    }
}

impl std::error::Error for EngineError {}

pub type Result<T> = std::result::Result<T, EngineError>;
