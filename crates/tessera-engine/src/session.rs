//! Engine construction and session authorisation.
//!
//! [`Engine::open`] runs the bundle read protocol, replays the WAL, seeds the I9 allocator, and
//! assembles the first [`Generation`]. [`Engine::authorise`] turns a credential into a
//! [`Session`]: the plugin's granted descriptors are resolved against the bundle dictionary
//! (an unknown descriptor is simply unsatisfied, never an error — the dictionary is the
//! authority on which descriptors exist), and the resulting term set is unioned into a mask
//! fragment via [`FragmentCache`] — this union *is* the authorisation decision (I2).

use std::io;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;
use std::time::{SystemTime, UNIX_EPOCH};

use arc_swap::ArcSwap;
use rand::rngs::OsRng;
use rand::RngCore;
use rustc_hash::FxHashSet;
use sha2::{Digest, Sha256};

use tessera_authz::{Dict, FragmentCache, FragmentCacheError, FrozenFragment, PostingsReader};
use tessera_lifecycle::command::UnallocatedRow;
use tessera_lifecycle::wal::{ChangeOp, WalError};
use tessera_lifecycle::OverlayError;
use tessera_plugin::{Descriptor, Plugin, PluginError};
use tessera_store::manifest::CurrentPointer;
use tessera_store::read::open_bundle;
use tessera_store::{Bundle, StoreError};
use tessera_types::{EntityId, IdentityError, IdentityKey, TermId, TesseraId};

use crate::cache::RowProjectionCache;
use crate::pins::{self, GeometryRefused, PinManager, PinStats, Reclaimed};
use crate::write::WritePath;
use crate::{Generation, GenerationHandle};

/// Engine-wide configuration — the subset of SA §7's `[disclosure]`/`[serve]` sections the engine
/// itself reads. `tessera-server` parses `tessera.toml`; this is the shape [`Engine::open`]
/// consumes.
#[derive(Debug, Clone, Copy)]
pub struct EngineConfig {
    /// How long a freshly minted session token remains valid, in seconds.
    pub token_max_lifetime_secs: u64,
    /// The hard cap on a viewport request's `k`. [`Engine::viewport`] clamps to this defensively
    /// even though the server is expected to enforce it at the HTTP boundary too.
    ///
    /// **This is the MACHINE ceiling** — GPU, transport, handle table — and it is the number the
    /// drawn-mark budget spec's probes calibrate. It is deliberately *not* the same knob as
    /// [`Self::k_max_marks`]: conflating them would mean that raising this on transport evidence
    /// silently dissolved §7.2's cap clause and the per-tile work bound with it.
    pub max_k: usize,
    /// §7.2's floor clause, `k_min`: the minimum marks a non-empty tile draws, whatever the
    /// threshold says. **This is the I7 guarantee** — it is what stops the sparsest principals'
    /// maps going blank — and it may not be removed as an optimisation. Provisional value 2 (density
    /// memo §4), pending that memo's §0 visual experiments.
    ///
    /// **Must be at least 1.** At 0 the floor clause is switched off and a tile whose visible items
    /// all sit above θ serves nothing — I7 gone, silently. `tessera-server`'s config loader refuses
    /// to start on `k_min = 0` (`ConfigError::FloorClauseDisabled`) rather than clamping, so no
    /// `tessera.toml` can reach that state; an embedder constructing this struct directly is on its
    /// own honour, which is why the constraint is stated here rather than only in the loader.
    pub k_min: usize,
    /// §7.2's cap clause, `K_max`: the most marks any one tile draws.
    ///
    /// **This is the OVERPLOT ceiling**, not the machine ceiling — density memo §4 sizes it at 128
    /// from ink coverage at ~80x80 px per tile, explicitly "overplot-bound, not machine-bound". A
    /// client may request `k <= k_max_marks`; the effective cap is the smaller. Provisional,
    /// pending the memo's §0 visual experiments.
    pub k_max_marks: usize,
    /// θ's anchor target: the number of marks the *mean occupied tile* should draw at any depth.
    /// θ_0 is derived as `theta_target_marks * 2^64 / V_total` and progresses `x4` per depth, which
    /// is what makes the per-tile expectation depth-stable. Provisional value 16 (density memo §4).
    ///
    /// Raising this above a session's total visible count saturates θ, which turns selection into
    /// "serve every visible row up to the cap" — the configuration tests use when they mean to
    /// assert masking rather than density.
    pub theta_target_marks: u64,
    /// The largest `underlay_offset` a request may ask for (§3.3 sub-cell counts). The sub-cell
    /// depth is `zoom + offset`, clamped to 16.
    pub max_underlay_offset: u8,
    /// The most tiles one `/v1/viewport` may span — an **availability** bound, see
    /// [`EngineError::TooManyTiles`]. A viewport is expected to draw a few hundred tiles; the
    /// default leaves generous headroom over that while keeping the worst case bounded.
    pub max_tiles_per_request: usize,
    /// The hard ceiling on sub-cells in one response. The underlay multiplies the (already-bounded,
    /// see [`Self::max_tiles_per_request`]) tile set by `4^offset`, so without this a single request
    /// can still ask for ~77k `count_range` calls and blow the 10 ms p99 latency gate.
    pub max_underlay_cells: usize,
    /// D-D: the size of `Engine::open`'s single shared `rayon::ThreadPool`, which every admitted
    /// request's tile loop `install`s onto (`Engine::viewport`, D-F). No second throttle exists
    /// inside the engine — `tessera-server`'s admission gate already bounds how many
    /// requests are concurrently *in* the engine at all, so this is sized to fill the machine, not
    /// to further divide it.
    ///
    /// Mirrors `tessera-server::config`'s `serve.compute_threads` (D-B, same knob, same default —
    /// [`default_compute_threads`]) so an embedder constructing this struct directly gets the same
    /// "fill the machine" behaviour the server's config loader enforces. Unlike the server's config
    /// loader, this struct does not refuse `0` itself (there is no fail-closed startup path at this
    /// layer to refuse *through*) — `rayon::ThreadPoolBuilder::num_threads(0)` falls back to
    /// rayon's own default (`RAYON_NUM_THREADS` or the logical core count), so a `0` here is
    /// harmless rather than a zero-width pool that can run nothing.
    pub compute_threads: usize,
    /// Lifecycle §2.2's pin TTL, in seconds — how long a pin stays resolvable once the generation
    /// it names has been superseded. Handed to [`crate::pins::PinManager`] at open, where it bounds
    /// the drain list: it is enforced at resolve as well as at reclaim, and it is (with
    /// `crate::pins::DRAIN_DEPTH_MAX`, and with a reclaim pass actually running) what bounds
    /// retention. Mirrors `tessera-server::config`'s `serve.pin_ttl_secs`, whose doc carries the
    /// page-cache argument that sizes it.
    pub pin_ttl_secs: u64,
    /// Lifecycle §2.2's per-session pin cap: the most **superseded** geometries one session may
    /// hold resolvable at once. Presenting a further one is
    /// [`EngineError::PinCapExceeded`] (422). Mirrors `tessera-server::config`'s
    /// `serve.pins_per_session_max`.
    ///
    /// **Counted at presentation of a drained pin, never at mint** — see
    /// `crate::pins::PinManager::resolve_drained`. A mint always names the live generation, of
    /// which a session can hold exactly one and which holds nothing alive that is not already live,
    /// so a mint consumes no resource and there is nothing there to cap; refusing at mint would
    /// `422` an ordinary unpinned viewport and would put the drain lock on the common request path.
    /// This is **not** the page-cache defence — `crate::pins::PinManager::pins_per_session_max`
    /// says what it does and does not bound.
    pub pins_per_session_max: usize,
}

/// D-D's default: fill the machine. Identical reasoning and identical fallback (`1`, never
/// propagated — see `tessera-server::config::default_compute_threads`'s doc) to the server's own
/// default, kept as a free function here so every non-server construction site (tests, benches,
/// examples, embedders) gets the same "fill the machine" behaviour without having to know the
/// number itself.
pub fn default_compute_threads() -> usize {
    std::thread::available_parallelism().map_or(1, std::num::NonZeroUsize::get)
}

/// One authorised viewer session: the credential's granted term set and the mask fragment it
/// unions to (I2 — the fragment *is* the authorisation decision, computed once here and reused,
/// never recomputed per viewport).
///
/// `handles` (a per-session entity-ID → wire-`Handle` table) is deliberately absent: that state
/// is owned by `tessera-wire`, which must not gain a dependency on this crate's
/// `EntityId` (I10) any more than this crate should depend on `tessera-wire`. `tessera-server`
/// holds a session's handle table alongside, not inside, this struct.
pub struct Session {
    /// Bearer token: 32 random bytes, hex-encoded.
    pub token: String,
    /// A process-local identity for this session, distinct from `token` — used as (part of) the
    /// row-projection cache key (`(token_id, slice, segments_version)`, shared-context
    /// constraint 8) so the cache never has to hash or compare the full token string.
    pub token_id: u64,
    /// The credential's granted terms, resolved to bundle-relative `TermId`s. An unknown
    /// descriptor (no dictionary entry) is simply absent here — never an error.
    pub satisfied: FxHashSet<TermId>,
    /// The materialised mask fragment (I2): the union of every satisfied term's postings.
    pub fragment: Arc<FrozenFragment>,
    /// Unix timestamp (seconds) after which this session is no longer valid.
    pub expires_at: u64,
}

/// Engine-level failures. Every variant here is fail-closed (Global Constraint 3): none of them
/// hand back a partial or best-effort result.
#[derive(Debug)]
pub enum EngineError {
    Store(StoreError),
    Wal(WalError),
    Overlay(OverlayError<StoreError>),
    Plugin(PluginError),
    Io(io::Error),
    /// A presented pin's `(prefix, segments_version)` does not match the live generation (I11) —
    /// maps to HTTP 410 at the server boundary.
    PinExpired,
    /// This session already holds `pins_per_session_max` pins and asked for another (lifecycle
    /// §2.2's per-session cap). Maps to **422 `contract`** — contracts §3.1's 422 row is
    /// "malformed request, **bounds exceeded**, unknown filter operand", the same row
    /// [`Self::TooManyTiles`] and [`Self::UnderlayRefused`] take. Not a 429: a cap that clears
    /// only when a pin TTLs out is not backpressure, and `Retry-After: 1` would be a lie at a
    /// five-minute TTL.
    ///
    /// Raised where a session would come to hold more than its configured number of pins;
    /// `a_session_cannot_exceed_its_pin_cap` is the test. It is a named variant rather than a
    /// fall-through so that `map_engine_error`'s catch-all cannot report a caller-fixable bound as
    /// a fail-closed 500. Both counts are the caller's own and the configured limit; no corpus fact
    /// rides on this error.
    PinCapExceeded {
        held: usize,
        limit: usize,
    },
    /// A viewport request named a slice this bundle doesn't have.
    UnknownSlice(String),
    /// A slice with more than one segment. `tile_ranges` returns **segment-local** row indices
    /// (contracts §2.4), while the slice's mask is built from one `Permutation` addressing
    /// exactly one segment's row space — the build produces exactly one segment per
    /// (partition, slice). Summing `count_range`/`iter_range` over a second segment's
    /// ranges through that same row space would silently mis-count or mis-index rows belonging to
    /// a different segment; there is no segment-row offset table to fold them together correctly
    /// yet, so this fails closed rather than produce a wrong (not even necessarily *obviously*
    /// wrong) answer.
    MultiSegmentSlice(String),
    /// A slice carried by more than one partition.
    ///
    /// The symmetric case to [`Self::MultiSegmentSlice`], and it fails closed for the symmetric
    /// reason: `Engine::viewport` resolves a slice by taking the first partition that carries the
    /// id, and θ's anchor plus every rank is then computed over **that partition alone**. Design
    /// §12.3 requires the anchor to be session-global across partitions — a per-partition anchor
    /// makes "below the cut" mean different things in different partitions, so the coordinator's
    /// union stops computing §7.2's definition. The build emits exactly one partition, so this
    /// is unreachable today; serving a §12 bundle half-masked with no error is what it prevents.
    MultiPartitionSlice(String),
    /// A bundle-level file (`CURRENT`, a plugin hash) was not the shape this engine expects.
    Malformed(String),
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
    /// This session's row projection for `(token_id, slice, segments_version)` is being
    /// built by a concurrent request right now. **Waiters do not park** (`tessera-bench`'s load
    /// arm measures the alternative): a parked waiter would hold the server's admission budget
    /// while burning zero CPU, so this call returns immediately and the caller is expected to
    /// retry.
    /// **⊘ Specified, not implemented:** the intended mapping is HTTP 429 with `Retry-After`. What
    /// happens instead is the server's fail-closed 500 arm — honest, never fail-open, but not the
    /// retryable signal a client can act on.
    ProjectionBuilding,
    /// This credential's mask fragment (lifecycle §3.3), keyed by the canonical `(bundle_identity, auth_plugin_hash, satisfied terms)` key, never
    /// `auth_data_hash` — see `tessera_authz::FragmentCache::get_or_build`'s doc — is being built
    /// by a concurrent `authorise` call right now. Same non-blocking-waiters rule and the same
    /// unbuilt mapping as [`Self::ProjectionBuilding`] (⊘): this call does not wait, the caller
    /// retries, and the server takes the fail-closed 500 arm.
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
            EngineError::Store(e) => write!(f, "store error: {e}"),
            EngineError::Wal(e) => write!(f, "wal error: {e}"),
            EngineError::Overlay(e) => write!(f, "overlay error: {e}"),
            EngineError::Plugin(e) => write!(f, "plugin error: {e}"),
            EngineError::Io(e) => write!(f, "io error: {e}"),
            EngineError::PinExpired => write!(f, "pin expired"),
            EngineError::PinCapExceeded { held, limit } => write!(
                f,
                "this session already holds {held} pins, at its configured maximum of {limit}; \
                 reuse a pin it holds, or let one expire"
            ),
            EngineError::UnknownSlice(slice) => write!(f, "unknown slice '{slice}'"),
            EngineError::MultiSegmentSlice(slice) => write!(
                f,
                "slice '{slice}' has more than one segment, which this engine's row-space \
                 handling does not yet support (see EngineError::MultiSegmentSlice's doc)"
            ),
            EngineError::MultiPartitionSlice(slice) => write!(
                f,
                "slice '{slice}' is carried by more than one partition, which this engine's \
                 single-anchor selection does not yet support (see \
                 EngineError::MultiPartitionSlice's doc)"
            ),
            EngineError::Malformed(detail) => write!(f, "malformed: {detail}"),
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

/// The request-serving engine: one immutable [`Generation`] behind an atomically-swappable
/// pointer, plus the process-lifetime state that doesn't change on an overlay/bundle swap (the
/// dictionary, the postings reader, the fragment cache).
pub struct Engine {
    /// The live generation pointer. `Arc`-shared with [`WritePath`], which publishes every
    /// generation swap through this exact pointer: the write path owns the swap, the
    /// read paths own the load, and both must see one pointer or a swap would be invisible.
    pub(crate) generation: Arc<GenerationHandle>,
    pub(crate) plugin: Arc<dyn Plugin>,
    pub(crate) dict: Arc<Dict>,
    pub(crate) postings: Arc<PostingsReader>,
    pub(crate) fragment_cache: Arc<FragmentCache>,
    /// The row-projection cache — see [`RowProjectionCache`]'s own doc.
    pub(crate) row_projection_cache: RowProjectionCache,
    /// D-D: the ONE shared compute pool every admitted `viewport` request's tile loop `install`s
    /// onto (`Engine::viewport`). Built once, here, at open — never per request, and never a
    /// second pool anywhere else in this crate (no nested throttling). `pool.install` from more
    /// external (server-side) threads than this pool has workers only queues on rayon's injector;
    /// it does not deadlock.
    pub(crate) pool: rayon::ThreadPool,
    pub(crate) config: EngineConfig,
    next_token_id: AtomicU64,
    /// The pin seam (I11) — see [`PinManager`]. Stateless today; `Engine::viewport` resolves
    /// every request's pin through it rather than comparing fields inline.
    pub(crate) pins: PinManager,
    /// The write path: the WAL, the I9 allocator, the live external-id maps, the resolver's
    /// extension state and the idempotency index. Every mutating engine method below is
    /// a thin delegation to this; the read paths that need write-side state (`resolve_external_id`
    /// and its two siblings) compose over its read accessors, so this crate has one owner for each
    /// mutable field rather than two.
    pub(crate) write: WritePath,
    /// External ids established by the bundle's own extent, at open — immutable for the process
    /// lifetime.
    external_index: ExternalIdIndex,
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
    /// The effective serial/parallel fan-out threshold
    /// (`viewport::SERIAL_FALLBACK_MAX_ROWS`) this engine reads on every `viewport` call,
    /// defaulted at `open` to that constant and never otherwise written in production. Exists so
    /// `set_serial_fallback_max_rows_for_test` (below) has something per-`Engine` to override —
    /// see that method's doc for why this lives here rather than as global or thread-local state.
    /// `pub(crate)`: `viewport.rs`'s `Engine::viewport` (a different module, same crate) reads it
    /// on every request.
    pub(crate) serial_fallback_max_rows: AtomicU64,
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
        let bundle = open_bundle(bundle_root).map_err(EngineError::Store)?;

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

        let segments_version = partition.manifest.segments_version;
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

        // The sidecar is lazy for real: nothing here is opened, mapped or verified —
        // `ExternalIdSidecar::deferred_from_manifest` only reads already-parsed JSON manifest
        // data (paths and digests), never the filesystem. No extent descriptor, digest, ordinal
        // or file path is handed to this crate — the constructor takes the manifests and the
        // prefix directory and keeps everything else behind its own API.
        let external_index =
            ExternalIdIndex::open(&bundle.manifest, &partition.manifest, &prefix_dir)
                .map_err(EngineError::Store)?;

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
        let (overlay, buffer, write_state) = WritePath::reconstruct(
            wal_path,
            bundle.manifest.entity_id_high_water,
            &dict,
            |external_id| external_index.resolve(external_id),
        )?;

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
        let pool = rayon::ThreadPoolBuilder::new()
            .num_threads(config.compute_threads)
            .build()
            .map_err(|e| EngineError::ThreadPoolBuild(e.to_string()))?;

        // `Arc`-wrapped from the start: the `Engine` and its `WritePath` share this one
        // pointer, so a swap published by an acceptance is the swap every read path observes.
        let generation = Arc::new(ArcSwap::new(Arc::new(Generation {
            prefix,
            segments_version,
            watermark,
            bundle: Arc::new(bundle),
            overlay_version: 0,
            overlay: Arc::new(overlay),
            buffer: Arc::new(buffer),
        })));

        Ok(Engine {
            generation: Arc::clone(&generation),
            plugin,
            dict: Arc::clone(&dict),
            postings,
            fragment_cache,
            // Unbounded until `set_cache_bounds` is called. `tessera-server` calls it immediately
            // after `open`, having validated the figure; every other embedder (tests, benches,
            // examples) gets unbounded caches, which is what a read-only embedder wants.
            row_projection_cache: RowProjectionCache::new(u64::MAX),
            pool,
            config,
            next_token_id: AtomicU64::new(0),
            pins: PinManager::new(config.pin_ttl_secs, config.pins_per_session_max),
            write: WritePath::new(write_state, dict),
            external_index,
            identity_key,
            serial_fallback_max_rows: AtomicU64::new(crate::viewport::SERIAL_FALLBACK_MAX_ROWS),
        })
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

    /// Authorise a credential: `plugin.terms_of_auth` → dictionary lookup (unknown descriptors
    /// simply drop out, never an error) → `FragmentCache::get_or_build`. A zero-term credential
    /// (or one whose every descriptor is unknown) is a valid, zero-visibility session (R5) — not
    /// an error.
    ///
    /// D-G (lifecycle §3.3): `FragmentCache::get_or_build` single-flights concurrent same-key
    /// misses and doubles as an in-memory cache for warm hits (see its doc); a concurrent
    /// in-flight build on this exact canonical key surfaces here as `Err(EngineError::
    /// FragmentBuilding)` rather than blocking.
    pub fn authorise(&self, auth_data: &[u8]) -> Result<Session> {
        let auth_terms = self
            .plugin
            .terms_of_auth(auth_data)
            .map_err(EngineError::Plugin)?;

        let satisfied: FxHashSet<TermId> = auth_terms
            .terms
            .iter()
            .filter_map(|descriptor| self.dict.lookup(descriptor))
            .collect();

        let mut satisfied_sorted: Vec<TermId> = satisfied.iter().copied().collect();
        satisfied_sorted.sort_unstable();

        // The cache's caller obligation (`FragmentCache::get_or_build`'s doc): this hash must be
        // a function of the exact `auth_data` that produced `satisfied` above, which it is.
        let auth_data_hash: [u8; 32] = Sha256::digest(auth_data).into();

        let generation = self.generation.load();
        let fragment = self
            .fragment_cache
            .get_or_build(
                &satisfied_sorted,
                auth_data_hash,
                &self.postings,
                generation.watermark,
            )
            .map_err(|e| match e {
                FragmentCacheError::Building => EngineError::FragmentBuilding,
                FragmentCacheError::Io(io_err) => EngineError::Io(io_err),
            })?;

        let mut token_bytes = [0u8; 32];
        OsRng.fill_bytes(&mut token_bytes);
        let token = hex_encode(&token_bytes);

        let token_id = self.next_token_id.fetch_add(1, Ordering::Relaxed);

        let now = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .expect("system clock is before the Unix epoch")
            .as_secs();
        let expires_at = now + self.config.token_max_lifetime_secs;

        Ok(Session {
            token,
            token_id,
            satisfied,
            fragment,
            expires_at,
        })
    }

    /// Delegates to `WritePath::allocator_high_water`, which owns the allocator; see that
    /// method's doc.
    pub fn allocator_high_water(&self) -> u64 {
        self.write.allocator_high_water()
    }

    /// Publish a new row-space geometry, retiring the outgoing one onto the pin manager's drain
    /// list (I11, lifecycle §2.1–§2.3). Returns everything the depth trim and the reclaim pass
    /// removed — the cache pruner's hook, see [`Reclaimed`].
    ///
    /// **The single seam a geometry swap may go through**, and the only thing in this process that
    /// moves `segments_version`. A flush and a compaction are its production callers; until one
    /// exists, its callers are the tests that prove the drain list works — which is why it is a
    /// real API and not a test hook, a drain list with no producer being untestable.
    ///
    /// **It structurally cannot regress authorisation state.** `overlay`, `buffer` and
    /// `overlay_version` are carried forward *unchanged* from whatever generation is live at the
    /// instant of the compare-and-swap — this method has no parameter that could carry a stale one,
    /// which is what lets it exist as a public API at all. `overlay_version` in particular is
    /// carried, never bumped: bumping it on a geometry-only swap would falsely signal a change on
    /// lifecycle §1.2's *security-state* axis, which §8.5's cache keys read.
    ///
    /// **What it does NOT swap, and a flush must not assume otherwise.** `Engine`'s `postings`
    /// reader, `dict` and the `FragmentCache`'s `bundle_identity` are all bound at
    /// [`Engine::open`] for the process lifetime. This method is therefore a **compaction-shaped**
    /// publication: correct when the new prefix's term index and dictionary are the same ones (a
    /// compaction rewrites the permutation, tile table, columns and candidate lists, and
    /// deliberately does *not* invalidate the term index or masks — §11.3), and **not sufficient
    /// for a flush that introduces new terms or new entities**, which would leave every
    /// subsequently-authorised session building its fragment from the old prefix's postings. That
    /// direction is conservative rather than fail-open — a newly flushed entity is absent from the
    /// stale fragment, so it goes unseen — but it is wrong, and a flush must widen this signature
    /// or swap those fields alongside.
    ///
    /// **This is the one publisher outside `write.rs`, and it is a genuine second publisher.**
    /// Every other generation swap in this process happens on the executor thread, which is what
    /// makes "one publisher" structural rather than a discipline (`write.rs`'s module doc;
    /// `scripts/check-layers.sh` rule 1). The compare-and-swap below cannot itself *lose* a
    /// concurrent overlay swap — but the executor's unconditional `store` can lose the geometry
    /// published here, after which the live generation is one whose identity this method has
    /// already retired, and a later `prune_generation` evicts the live generation's own
    /// projections. The identity check below narrows that window; **nothing in this file closes
    /// it**, and it must not be read as a defence that makes the executor's `store` safe. Closing
    /// it means the geometry publisher running on the writer thread, which lifecycle §1.3 requires
    /// of a flush anyway.
    ///
    /// `prefix`, `segments_version` and `watermark` are the values from the new prefix's own
    /// SEGMENTS manifest; they are taken separately from `bundle` rather than read out of it
    /// because the caller — a flush or a compaction publication — is the thing that decides what
    /// `n` the new manifest carries. `segments_version` must strictly increase; see
    /// [`GeometryRefused`] and `pins::check_publishable` for why that is a refusal and not a
    /// warning.
    pub fn publish_geometry(
        &self,
        prefix: String,
        segments_version: u64,
        watermark: u64,
        bundle: Arc<Bundle>,
    ) -> std::result::Result<Vec<Reclaimed>, GeometryRefused> {
        // An explicit compare-and-swap loop rather than `ArcSwap::rcu`, for two reasons. The guard
        // has to be evaluated against the generation actually being replaced, which means inside
        // the loop and able to abandon it — `rcu`'s closure has no way to say no. And the retire
        // below must run on the generation the swap *actually* replaced, exactly once: `rcu` may
        // run its closure several times under contention, so retiring from inside it would push
        // duplicate — or entirely spurious — drain entries.
        let previous = loop {
            let live = self.generation.load_full();
            pins::check_publishable(&live, &prefix, segments_version)?;
            let next = Arc::new(Generation {
                prefix: prefix.clone(),
                segments_version,
                watermark,
                bundle: Arc::clone(&bundle),
                overlay_version: live.overlay_version,
                overlay: Arc::clone(&live.overlay),
                buffer: Arc::clone(&live.buffer),
            });
            // The one generation publication outside `write.rs`, and it is deliberately
            // temporary. The executor thread is the sole publisher — structurally, because a
            // `load_full` + `store` racing a geometry publication loses it and strands the LIVE
            // generation on the drain list. `check-layers.sh` rule 1 enforces that, and the marker
            // on the statement below is its single permitted exception. The rule **counts** those
            // markers and fails on a second, so the cheap escape (add another) is exactly as
            // visible as the honest fix.
            //
            // **Why it is tolerable meanwhile.** Nothing in `tessera-server` calls
            // `publish_geometry`; it exists because nothing else moves `segments_version`, so
            // without it the drain list is untestable. It is a CAS in a retry loop, not an
            // unconditional store, so it cannot itself *lose* a publication — but the executor's
            // stores can still clobber it, which is why the retire below reads the identity
            // observed live rather than trusting this swap.
            //
            // **What retires it.** Lifecycle §1.3 already requires a flush's swap-only publication
            // step to run on the lifecycle thread. When a flush lands, this becomes a `Command` the
            // executor performs and the marker goes with it. Leaving a second publisher in place
            // because "the CAS is safe" reintroduces the race against every publication the
            // executor makes concurrently.
            let seen = arc_swap::Guard::into_inner(self.generation.compare_and_swap(&live, next)); // PUBLISHER-EXEMPT(2.2)
            if Arc::ptr_eq(&live, &seen) {
                break live;
            }
        };

        // Retire against the geometry identity that is live *now* — the observation
        // `PinManager::retire` decides on, not the identity offered above. The compare-and-swap
        // proves `previous` was superseded at the instant it ran; a `WritePath` `store` that began
        // before it and lands after it makes `previous`'s geometry live again under a **fresh
        // `Arc`** (`write.rs` copies `prefix` and `segments_version` forward), and draining a live
        // identity is what `retire`'s guard refuses.
        //
        // Pointer identity cannot express that: an `Arc::ptr_eq` here is a no-op, because nothing
        // ever re-`store`s `previous`'s own pointer, so the comparison is false both in the case it
        // would be meant to catch and in every other. It does not mitigate the clobber.
        //
        // **This narrows the window; it does not close it.** A store landing after this load is
        // unobserved. The obligation above is therefore load-bearing, not belt-and-braces.
        let live_now = self.generation.load_full();
        let mut reclaimed =
            self.pins
                .retire(&previous, &live_now.prefix, live_now.segments_version);
        // Reclaim *after* retiring, so the list is self-bounding for as long as geometry keeps
        // moving. This is not a substitute for a periodic pass — see `reclaim_pins`.
        reclaimed.extend(self.pins.reclaim());
        // Prune the projections of every geometry this call released — the trim above and
        // the reclaim pass alike. Coupled to the `Reclaimed` values, never to the swap; see
        // `Engine::prune_reclaimed`.
        self.prune_reclaimed(&reclaimed);
        Ok(reclaimed)
    }

    /// One reclaim pass over the pin drain list — remove → verify → drop (lifecycle §2.1).
    ///
    /// The lifecycle thread's periodic call, and the cache pruner's other hook, since
    /// [`Reclaimed::segments_version`] is exactly the row-projection cache key component to prune.
    ///
    /// **A periodic caller is required, not optional.** This is the only thing that releases a
    /// superseded bundle's memory; the TTL bounds availability at resolve and frees nothing. Until
    /// one exists, memory is released only by the next [`Self::publish_geometry`], so a process
    /// that publishes once and goes quiescent holds a whole superseded bundle indefinitely — at
    /// drain depth 1 — *at* `DRAIN_DEPTH_ALARM`, which alarms only above it. [`PinStats::oldest_retired_secs`]
    /// is the gauge that makes that state visible.
    pub fn reclaim_pins(&self) -> Vec<Reclaimed> {
        let reclaimed = self.pins.reclaim();
        self.prune_reclaimed(&reclaimed);
        reclaimed
    }

    /// Prune the row-projection cache for every geometry a reclaim pass released.
    ///
    /// **The licence to prune is a [`Reclaimed`] value, not any particular method**, and that is
    /// the whole of the coupling argument. `Reclaimed`s are produced at three sites — this
    /// method's caller, [`Self::publish_geometry`]'s drain-depth trim, and the reclaim pass
    /// `publish_geometry` runs itself — and the safety property is identical at all three: the
    /// drain entry naming that `segments_version` is gone, so `PinManager::resolve_drained` now
    /// returns `PinExpired` and no request can produce that key again. Hanging the prune off
    /// `reclaim_pins` alone would leave the other two routes reclaiming geometries whose
    /// projections are never freed — and since nothing calls `reclaim_pins` periodically yet
    /// (see its doc), those are in practice the routes that fire.
    ///
    /// **Never from the swap.** Between a swap and the reclaim, the superseded geometry is still
    /// resolvable from the drain list, so a swap-triggered prune deletes exactly the key an
    /// outstanding pin is about to ask for. See `RowProjectionCache::prune_generation`.
    ///
    /// The window this does *not* close, stated rather than implied: a request that resolved its
    /// pin before the drain entry was removed can construct that key after this prune and
    /// re-publish it. That entry is that session's own projection over the geometry it pinned,
    /// reachable by nobody else, and the byte bound reclaims it. It is a bounded memory effect,
    /// never a disclosure — removal cannot widen a mask.
    fn prune_reclaimed(&self, reclaimed: &[Reclaimed]) {
        for entry in reclaimed {
            self.row_projection_cache
                .prune_generation(entry.segments_version);
        }
    }

    /// Drop every cached row projection belonging to `token_id` — the revoke hook.
    ///
    /// Returns how many entries were removed, which is what
    /// `revoke_prunes_the_token` asserts on. See `RowProjectionCache::prune_token` for why this is
    /// memory hygiene rather than a disclosure control, and for the cost of the pass.
    pub fn prune_token(&self, token_id: u64) -> usize {
        self.row_projection_cache.prune_token(token_id)
    }

    /// Bound both caches, and the only route by which the two config keys reach them.
    ///
    /// **Not an `EngineConfig` field, deliberately** *(and this cost a design revision)*.
    /// `EngineConfig` is `Copy` with no `Default` and is built by *exhaustive* struct literal at
    /// fifteen sites, three of which are in `crates/tessera-engine/tests/viewport.rs` — a file this
    /// stage's allowlist marks `[frozen]` for every track. Adding a field there would have made the
    /// workspace uncompilable with no in-allowlist repair. `Engine::start_write_executor` met the
    /// same wall with `ingest_queue_bound` and answered it the same way; this follows that
    /// precedent rather than inventing a second one.
    ///
    /// Called by `tessera_server::prepare` immediately after [`Self::open`], *after* it has
    /// validated both figures against `expected_concurrent_sessions`. An embedder that never calls
    /// this gets unbounded caches, which is stated here rather than
    /// silently assumed, and is the same posture `EngineConfig::k_min` documents for a constraint
    /// only the server's loader enforces.
    pub fn set_cache_bounds(&self, row_projection_bytes: u64, fragment_bytes: u64) {
        self.row_projection_cache
            .set_bound_bytes(row_projection_bytes);
        self.fragment_cache.set_memory_bound(fragment_bytes);
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
                 configured soft limit. It alarms; it does not act — there is no fold until stage \
                 2.3"
            );
        }
    }

    /// The live overlay's entry count — the gauge `/control/status` publishes beside the soft
    /// limit's alarm counter. Read straight off the current generation, so it needs no counter of
    /// its own and cannot drift from what a request would compose against.
    pub fn overlay_depth(&self) -> usize {
        self.generation.load().overlay.len()
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
        self.fragment_cache.stats()
    }

    /// Times the fragment cache has actually re-unioned postings (rather than reopening a
    /// digest-verified `.frag` sidecar or hitting the in-memory tier). The observable that
    /// separates an in-memory eviction from a genuinely cold rebuild.
    pub fn fragment_cache_rebuilds(&self) -> u64 {
        self.fragment_cache.rebuild_count()
    }

    /// The canonical cache key for `satisfied` under this engine's bundle and plugin identity — the
    /// only way to name a fragment entry from outside, and therefore what [`Self::evict_fragment`]
    /// takes. Pure; reveals nothing the caller did not supply.
    pub fn fragment_canonical_key(&self, satisfied: &[TermId]) -> [u8; 32] {
        self.fragment_cache.canonical_key_for(satisfied)
    }

    /// Drop one entry from the fragment cache's **in-memory** tier; returns whether it was there.
    /// The digest-verified `.frag`/`.meta` pair is deliberately left on disk — see
    /// `FragmentCache::evict`, which carries that argument and the caveat that a live `Session`
    /// holding the fragment keeps its mapping alive regardless.
    ///
    /// The conformance command is the intended caller.
    pub fn evict_fragment(&self, key: &[u8; 32]) -> bool {
        self.fragment_cache.evict(key)
    }

    /// The pin drain-list gauges — see [`PinStats`]. Published on `/control/status` as `pins`.
    pub fn pin_stats(&self) -> PinStats {
        self.pins.stats()
    }

    /// The number of cached row-space projection slots currently held (`Building` and `Ready`
    /// both counted) — exposed for tests confirming `Engine::item`'s entity-space visibility test
    /// never constructs one: this must stay `0` across drill-down calls, warm or
    /// cold, unlike `Engine::viewport`'s path, which populates this cache deliberately.
    pub fn row_projection_cache_len(&self) -> usize {
        self.row_projection_cache.len()
    }

    /// Whether the external-id sidecar has opened any extent (or its locator) yet — exposed for
    /// tests confirming `Engine::open` never touches it — the per-extent laziness guarantee.
    pub fn external_id_sidecar_is_open(&self) -> bool {
        self.external_index.0.is_open()
    }

    /// The plugin this engine was opened with — the `/control/ingest` handler calls
    /// `terms_of_label` through this to turn an item's `access` bytes into descriptors.
    pub fn plugin(&self) -> &Arc<dyn Plugin> {
        &self.plugin
    }

    /// The plugin's declared sizing bounds — the ingest handler consults these to
    /// decide `over_bound`, never to exclude an item (bounds warn, never exclude — design §6.2
    /// r16).
    pub fn declared_bounds(&self) -> tessera_plugin::DeclaredBounds {
        self.plugin.declared_bounds()
    }

    /// Resolve raw term descriptors to `TermId`s (dictionary hit → durable bundle-relative id;
    /// miss → an id interned in this process's extension state, resumed across calls).
    ///
    /// **Caller obligation — the durability-ordering exemption.** Every other resolution site
    /// resolves *after* the record carrying the descriptors is durably appended and fsynced, so a
    /// batch whose append fails cannot leave the live resolver a step ahead of what a replay would
    /// reconstruct. `/control/ingest` is the one structural exception: signature-sorted assignment
    /// (I9/§11.1) needs each item's terms to compute its sort key before its `WalRow` can be
    /// framed at all. Judged safe because an extension id is by construction unsatisfiable by any
    /// session, so a live/replay mismatch renumbers bookkeeping and never a visibility outcome —
    /// the full argument, and why it is not merely convenient, is at `WritePath::resolve_terms`.
    ///
    /// *(Restated here rather than only cross-referenced: `WritePath` is `pub(crate)`, so rustdoc
    /// renders none of its docs for a reader of this public API, and a bare pointer to an invisible
    /// page is not an obligation a caller can honour.)*
    pub fn resolve_terms(&self, descriptors: &[Descriptor]) -> Vec<TermId> {
        self.write.resolve_terms(descriptors)
    }

    /// Resolve an external id to its `EntityId`, checking every item established live (bundle
    /// replay's own `IngestBatch` rows, plus every `/control/ingest` batch accepted since) before
    /// falling back to the bundle's own `entities/external-ids-0.arrow` extent.
    ///
    /// **Fallible**, and that is the point: a real sidecar failure — digest
    /// mismatch, out-of-order extent, corrupt locator — propagates as `Err` rather than panicking
    /// inside `ExternalIdIndex::resolve`. A `/control/changes` request naming
    /// an external id backed by a corrupt sidecar gets a `500`, never a silent "unknown" *or* a
    /// panicked worker.
    pub fn resolve_external_id(
        &self,
        external_id: &[u8],
    ) -> std::result::Result<Option<EntityId>, StoreError> {
        if let Some(entity) = self.write.established_entity(external_id) {
            return Ok(Some(entity));
        }
        self.external_index.resolve(external_id)
    }

    /// Batch form of [`Self::resolve_external_id`] for `/control/ingest`'s duplicate check
    /// (contracts §3.1 r6): live map first for the *whole* batch (Important I-8 — `established`
    /// holds every id ingested since the build, which the sidecar cannot see at all, and is
    /// exactly where a retried client batch's duplicate lives), then one batched, sorted sidecar
    /// call for whatever residual keys the live map didn't resolve — each bundle extent is opened
    /// at most once regardless of batch size, never once per row.
    ///
    /// Returns one `Option<EntityId>` per input, in the caller's given order.
    pub fn resolve_external_ids(
        &self,
        external_ids: &[Vec<u8>],
    ) -> std::result::Result<Vec<Option<EntityId>>, StoreError> {
        let mut results: Vec<Option<EntityId>> = self.write.established_entities(external_ids);

        let residual_positions: Vec<usize> = results
            .iter()
            .enumerate()
            .filter_map(|(i, r)| if r.is_none() { Some(i) } else { None })
            .collect();
        if residual_positions.is_empty() {
            return Ok(results);
        }
        let residual_keys: Vec<Vec<u8>> = residual_positions
            .iter()
            .map(|&i| external_ids[i].clone())
            .collect();
        let residual_results = self.external_index.resolve_many(&residual_keys)?;
        for (pos, resolved) in residual_positions.into_iter().zip(residual_results) {
            results[pos] = resolved;
        }
        Ok(results)
    }

    /// `entity -> external_id` for drill-down (`/v1/items`). Ordering mirrors
    /// `resolve_external_id`'s live-map-first rule, running the other way: post-build ingest has
    /// no locator slot and no extent entry, so the live map (`established_inverse`) is consulted
    /// first — Important I-9. `Ok(None)` means "this item genuinely has no caller external id", a
    /// legitimate state since `external_id` is optional on ingest; it must never mean "I could
    /// not find out". A `/v1/items` entity that is below the live high-water, past this bundle's
    /// locator, and unknown to the live map is an inconsistency, not an absent external id, and
    /// fails closed as `Err(StoreError::InvalidSidecar)` — see
    /// `ExternalIdSidecar::external_id_of_checked`'s doc.
    pub fn external_id_of(
        &self,
        entity: EntityId,
    ) -> std::result::Result<Option<Vec<u8>>, StoreError> {
        if let Some(external_id) = self.write.established_external_id(entity) {
            return Ok(Some(external_id));
        }
        self.external_index
            .external_id_of_checked(entity, self.allocator_high_water())
    }

    /// The body hash and per-row entity ids a batch id was previously accepted with, if any — the
    /// idempotency check for `/control/ingest`'s replay rule: equal hash -> 200 no-op (returning
    /// the same `tessera_id`s, via the entity ids here); different hash -> 409.
    ///
    /// **An accelerant, never the authority.** The same check runs again on the executor, which is
    /// the only place it can be race-free (see `WritePath`'s executor). A handler consulting this
    /// is saving a queue round-trip on the common case, not deciding anything.
    pub fn accepted_batch(&self, batch_id: &str) -> Option<([u8; 32], Vec<EntityId>)> {
        self.write.accepted_batch(batch_id)
    }

    /// Compute the wire `tessera_id` for `entity` under this deployment's current shard id and
    /// identity key (contracts §2.6/§3.4 r6). `/control/ingest`'s 200 response returns each
    /// accepted row's `tessera_id` this way rather than its raw `EntityId` (I10: entity ids never
    /// cross the trust boundary).
    ///
    /// **Fallible, not `.unwrap()`-able**: `IdentityKey::forward` refuses an entity at or above
    /// `u32::MAX` (Important I-1). The I9 allocator's ceiling makes that unreachable in practice
    /// for any entity this method is ever called with, but this stays a typed error rather than a
    /// panic — an internal invariant violation must fail closed (500), never crash the request
    /// thread or silently truncate.
    pub fn tessera_id_of(&self, entity: EntityId) -> std::result::Result<TesseraId, IdentityError> {
        let generation = self.generation.load_full();
        self.identity_key
            .forward(generation.bundle.manifest.identity.shard_id, entity)
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
        self.write.start_executor(
            generation,
            queue_bound,
            #[cfg(feature = "fault-injection")]
            None,
        )
    }

    /// As [`Engine::start_write_executor`], with a fault switchboard armed. Test builds only.
    #[cfg(feature = "fault-injection")]
    pub fn start_write_executor_with_faults(
        &mut self,
        queue_bound: usize,
        faults: Arc<tessera_lifecycle::faults::FaultSwitchboard>,
    ) -> std::result::Result<(), crate::write::ExecutorStartError> {
        let generation = Arc::clone(&self.generation);
        self.write
            .start_executor(generation, queue_bound, Some(faults))
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

    /// Submit an ingest batch and wait for its receipt. Rows arrive **unallocated**: entity ids are
    /// assigned on the executor, at the close of the commit window this submission lands in.
    ///
    /// Blocking — a tokio handler must call this inside `spawn_blocking`.
    pub fn accept_ingest(
        &self,
        rows: Vec<UnallocatedRow>,
        batch_id: String,
        body_hash: [u8; 32],
    ) -> std::result::Result<Vec<EntityId>, crate::write::AcceptError> {
        self.write.accept_ingest(rows, batch_id, body_hash)
    }

    /// Submit one `/control/changes` entry and wait for its receipt.
    ///
    /// **An `Err` does not mean nothing happened**: for `Delete`/`Suppress` a WAL failure still
    /// applies the change before returning (lifecycle §4). See `ExecError::Wal`.
    ///
    /// A caller with a whole request's worth of changes wants [`Engine::submit_change`] instead —
    /// waiting between items is what reduces the deny lane's group commit to one entry per window.
    pub fn accept_change(
        &self,
        external_id: Vec<u8>,
        entity: EntityId,
        op: ChangeOp,
        raw_descriptors: Option<Vec<Vec<u8>>>,
    ) -> std::result::Result<(), crate::write::AcceptError> {
        self.write
            .accept_change(external_id, entity, op, raw_descriptors)
    }

    /// Enqueue one `/control/changes` entry **without waiting for its receipt**, so that a caller
    /// with several can have them all in the executor's queue at once.
    ///
    /// That queue depth is the whole precondition for the deny lane's group commit: a caller that
    /// waits between items leaves the executor one entry to gather, and one request of N denies
    /// costs N fsyncs. Read `PendingChange::wait` before treating either half's `Err` as "nothing
    /// happened".
    pub fn submit_change(
        &self,
        external_id: Vec<u8>,
        entity: EntityId,
        op: ChangeOp,
        raw_descriptors: Option<Vec<Vec<u8>>>,
    ) -> std::result::Result<crate::write::PendingChange, crate::write::AcceptError> {
        self.write
            .submit_change(external_id, entity, op, raw_descriptors)
    }
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

fn hex_encode(bytes: &[u8]) -> String {
    use std::fmt::Write;
    let mut s = String::with_capacity(bytes.len() * 2);
    for b in bytes {
        let _ = write!(s, "{b:02x}");
    }
    s
}

/// Resolve an external id to an [`EntityId`] via `tessera_store::ExternalIdSidecar` — a thin
/// newtype-free wrapper so callers in this crate keep using `EntityId` rather than the bare
/// `tessera_types`-free type the store crate returns, and so no `ExtentDesc`, digest, ordinal or
/// file path from the sidecar's own bookkeeping is ever named in this crate (Ruling B).
///
/// The sidecar is per-extent lazy: nothing is opened, mapped or digested until the
/// first resolution, and then only the one extent the key falls in — never the whole family.
/// This is the `resolve_from_bundle` seam `tessera_lifecycle::overlay::replay` uses,
/// authorisation-bearing because `/control/changes` denies whichever entity it resolves to — a
/// wrong resolution denies the wrong entity and leaves the intended target visible.
struct ExternalIdIndex(tessera_store::ExternalIdSidecar);

impl ExternalIdIndex {
    fn open(
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

    /// **Fallible, closing review round 4's Critical C3.** `resolve_from_bundle`'s closure
    /// signature in `tessera_lifecycle::overlay::replay` now takes a generic error parameter
    /// rather than a fixed `Option` — `tessera-lifecycle` does not depend on `tessera-store`, so
    /// the closure cannot name `StoreError` itself, but it can return any `Result<_, E>` and let
    /// the caller's `E` be inferred as `StoreError` here. A corrupt extent, a digest mismatch or
    /// a shuffled extent list now propagates as `Err(StoreError::InvalidSidecar)` through
    /// `replay`/`Engine::open`/`Engine::resolve_external_id`, rather than the previous panic —
    /// still fail-closed in effect, but no longer a panic in an async handler or at open.
    fn resolve(&self, external_id: &[u8]) -> std::result::Result<Option<EntityId>, StoreError> {
        self.0.resolve(external_id)
    }

    /// Batched form of [`Self::resolve`] — one sorted pass over `external_ids`, each extent
    /// opened at most once, rather than one open per row (`/control/ingest`'s duplicate check,
    /// contracts §3.1 r6).
    fn resolve_many(
        &self,
        external_ids: &[Vec<u8>],
    ) -> std::result::Result<Vec<Option<EntityId>>, StoreError> {
        self.0.resolve_many(external_ids)
    }

    /// `entity -> external_id`, distinguishing "genuinely has none" from "an inconsistency" — see
    /// `tessera_store::ExternalIdSidecar::external_id_of_checked`'s doc.
    fn external_id_of_checked(
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
    /// into `tile_result`: a `#[cfg(test)]`-visible injection point in the real per-tile path would
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
    /// anything `Engine::open` does when building one. The construction below is checked against
    /// `Engine::open`'s by inspection (both are a bare
    /// `rayon::ThreadPoolBuilder::new().num_threads(n).build()`, no further configuration either
    /// side) rather than by sharing code, which is what "identical construction" above means.
    #[test]
    fn a_panic_inside_the_shared_pool_propagates_to_the_caller() {
        let pool = rayon::ThreadPoolBuilder::new()
            .num_threads(2)
            .build()
            .expect("pool should build");

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
}
