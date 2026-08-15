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
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::Arc;
use std::time::{SystemTime, UNIX_EPOCH};

use arc_swap::ArcSwap;
use rand::rngs::OsRng;
use rand::RngCore;
use rustc_hash::FxHashSet;
use sha2::{Digest, Sha256};

use tessera_authz::{
    DeltaTier, Dict, FragmentCache, FragmentCacheError, FrozenFragment, PostingsReader,
};
use tessera_lifecycle::command::UnallocatedRow;
use tessera_lifecycle::wal::{ChangeOp, WalError};
use tessera_plugin::{Descriptor, Plugin, PluginError};
use tessera_store::manifest::CurrentPointer;
use tessera_store::read::open_bundle;
use tessera_store::vocabulary::Vocabularies;
use tessera_store::{Bundle, StoreError};
use tessera_types::{EntityId, IdentityError, IdentityKey, TermId, TesseraId};

use crate::cache::RowProjectionCache;
use crate::geometry::GeometryPublication;
use crate::write::{PublishGeometryError, WritePath};
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
    /// The flush tick, in seconds: the period at which geometry is published.
    ///
    /// **No longer bounded from below by a pin TTL.** It used to be, through
    /// `pin_ttl_secs < drain_depth_max × flush_max_age_secs`, and that relation went with the pin
    /// retention (`geometry-pinning.md` §0). What *does* bound it is the background refresh: a
    /// tick shorter than one refresh round (~0.7 s of pool time at the ~16 wide-grant entries a
    /// 2 GiB bound holds — `probes/2026-08-04-refresh-ladder/`) leaves every session permanently
    /// in the stale-serve window, and then two publications deep, where it rebuilds. That is a
    /// real floor and a different one; see `crate::refresh`.
    pub flush_max_age_secs: u64,
    /// `merge.max_merged_segment_bytes` — the largest total one row-space merge may consume, and
    /// therefore the ceiling on how far tiering can go before segments stop being mergeable.
    ///
    /// **`None` keeps the built-in default**; see [`default_merge_policy`] for what that is and why
    /// it is a fixed number rather than a derivation. An explicit value is what a deployment sets
    /// when it would rather spend pool memory than live segments — merge peak is a measured
    /// 4.4–4.9× the inputs' file bytes (`probes/2026-08-04-maintenance-memory/`), so this trades
    /// one directly for the other.
    ///
    /// **It reaches the merge policy from here.** `tessera-server` validated this key against
    /// write-path §7's base-segment relation from the day the key existed, and then had nowhere to
    /// send it: both `MaintenanceDeps` sites took `default_merge_policy()` unconditionally, so a
    /// configured value was checked and discarded. That is the inert key decision 0045 forbids,
    /// with the additional trap that validation made it look effective.
    pub max_merged_segment_bytes: Option<u64>,
    /// When a fold is dispatched with nobody asking for one — compaction §9's automatic trigger,
    /// as decision 0056 rules it.
    ///
    /// **[`CompactionSchedule::off`] is the value an embedder gets unless it says otherwise**, and
    /// that is deliberate: a fold is minutes to hours of IO, and starting one from a default nobody
    /// chose is not a decision this type may make on a caller's behalf. `tessera-server` applies
    /// §9's defaults, because it is where an operator can see and change them.
    pub compaction: crate::compact::CompactionSchedule,
}

/// The row-space merge's policy (write-path §7).
///
/// **`tier_width` 4, floor 16 MiB, cap 256 MiB.** The floor is where per-segment overheads stop
/// dominating, and clamping to it before taking the size class is what stops a deployment whose
/// flushes differ by a few bytes producing a size class per flush and merging nothing at all.
/// The cap bounds one merge's pool time and its write amplification.
///
/// **The base segment is excluded twice over.** `crate::merge::plan_merge` selects from the
/// **extent list**, and the base is the one segment with no extent (`permutation.bin` addresses
/// it), so no size makes it selectable. Write-path §7's **enforced relation** —
/// `max_merged_segment_bytes` strictly below the base segment's bytes — stands beside that and is
/// still refused at startup by `tessera-server`'s config loader. Both are kept deliberately: the
/// structural exclusion lives in one function and a refactor could lose it, and the startup
/// refusal is what would still be standing if it did.
/// The row-space merge's policy, with `EngineConfig::max_merged_segment_bytes` applied when set.
fn merge_policy(max_merged_segment_bytes: Option<u64>) -> tessera_store::merge::MergePolicy {
    tessera_store::merge::MergePolicy {
        tier_width: 4,
        segment_floor_bytes: 16 << 20,
        max_merged_segment_bytes: max_merged_segment_bytes.unwrap_or(256 << 20),
    }
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
    ///
    /// **Resolved once, at authorise, and never re-resolved in place** — see [`Session::is_stale`],
    /// whose third rule this is. A flush that promotes one of the descriptors that dropped out
    /// leaves this set as it was; the remedy is a new session, not a mutation of this one.
    pub satisfied: FxHashSet<TermId>,
    /// The materialised mask fragment (I2): the union of every satisfied term's postings, over
    /// the base and every delta tier live at the moment this session authorised.
    ///
    /// **It goes stale, and [`Engine::fragment_for`] is what brings it forward.** A flush publishes
    /// a delta tier and moves the generation's watermark, and composition treats entities *below*
    /// the watermark as fragment-resident — so a flushed entity is in neither this fragment nor the
    /// ingest buffer until the fragment is rebuilt at the new watermark. Nothing on the request
    /// path may read this field directly for that reason; see `Engine::fragment_for`.
    pub fragment: Arc<FrozenFragment>,
    /// `satisfied`, sorted — the [`tessera_authz::FragmentCache`] key component, kept rather than
    /// re-sorted per request so bringing the fragment forward costs no allocation on a hit.
    /// `Arc` so the row-projection cache's entry can carry it for the background refresh, which
    /// has no session registry to look it up in — see [`crate::cache::SessionGeometry`].
    pub(crate) satisfied_sorted: Arc<Vec<TermId>>,
    /// `sha256(auth_data)` — the cache's caller obligation, kept for the same reason.
    ///
    /// A digest of the credential, never the credential: this lives for the session's lifetime in
    /// a struct the server holds per connection, and the bearer secret must not.
    pub(crate) auth_data_hash: [u8; 32],
    /// Unix timestamp (seconds) after which this session is no longer valid.
    pub expires_at: u64,
    /// How many of the credential's granted descriptors had **no dictionary entry** at authorise,
    /// and the dictionary length they were resolved against — together, §3.3's staleness
    /// condition. See [`Session::is_stale`].
    ///
    /// Deliberately not `pub`. The boolean is the whole of what §3.3 specifies; the count is a
    /// fact about how many of this viewer's descriptors the corpus does not carry, which is
    /// strictly more than the boolean and would need its own leak-register argument before
    /// anything could put it on the wire.
    pub(crate) unresolved_count: usize,
    /// See [`Self::unresolved_count`].
    pub(crate) dict_len_at_authorise: u32,
    /// **The generation this session's `satisfied` was resolved against**, which is what
    /// [`Engine::fragment_for`] must hand `FragmentCache::get_or_build` alongside the frozen term
    /// set (#112).
    ///
    /// Distinct from [`Self::dict_len_at_authorise`] and not a duplicate of it: that one answers
    /// *has the dictionary grown since* and is the staleness condition's own input, where this one
    /// identifies the resolution. A length is a faithful stand-in for a generation only while the
    /// dictionary is append-only, which a fold breaks — `get_or_build`'s doc argues the difference,
    /// and it is why keying the memo on the length left a trap for a change nobody had made yet.
    pub(crate) segments_version_at_authorise: u64,
}

impl Session {
    /// **§3.3 — is this session's mask behind the corpus?** True iff this credential named a
    /// descriptor the dictionary did not carry at authorise *and* the dictionary has grown since.
    ///
    /// Two loads and a branch, evaluated lazily by whoever asks — never swept. Nothing walks the
    /// session registry when a flush promotes a term, which keeps the executor free of an
    /// O(sessions) publication step (decision 0035). `Dict` is generation-scoped (§3.2), so the
    /// current length comes off the generation the caller has already loaded once at request start
    /// (lifecycle §1.1's ordering invariant). Decision 0020 is untouched: a count and an integer
    /// are not authorisation data.
    ///
    /// **Over-reports in one direction, and that is the safe one.** A session with one unresolved
    /// descriptor is hinted whenever *any* term is promoted, not only its own; the false direction
    /// costs one voluntary re-authorisation. A session with nothing unresolved is never hinted.
    /// (The refinement — comparing digests of the unresolved descriptors against the promoted ones
    /// — is deliberately not built: it retains more and leaks more, confirming that *their*
    /// descriptor now exists where this says only that some term appeared.)
    ///
    /// **⊘ Specified, not implemented: the wire representation.** §3.3 states the internal
    /// condition only. No response carries this, and a client's policy for acting on it is
    /// client-facing work; what exists today is this predicate and its leak-register row (C21).
    ///
    /// Three rules keep it from becoming something it must not be:
    ///
    /// - **It moves in one direction only.** A stale session sees *fewer* items than its principal
    ///   is entitled to — fail-closed, which is what makes an advisory answer legitimate at all.
    ///   Grant changes are not covered, and **nothing may ever be wired to make a revocation take
    ///   effect through this**: decision 0025 governs rotation, and this is not a general "the mask
    ///   changed" channel.
    /// - **It is a hint, not an expiry.** Treating a stale session as expired would need no new
    ///   wire field and is already contractual under decision 0025 — and is rejected on load: it
    ///   forces every affected session to rebuild its fragment at one tick, and the next viewport
    ///   pays a **measured 1 277 ms** row projection at 10⁹. A hint spreads the same total work over
    ///   the interval. What bounds staleness for a client that ignores it already exists:
    ///   `token_max_lifetime_secs` caps every session's life. This is the fast path, not the safety
    ///   net.
    /// - **The only remedy is a new session; [`Self::satisfied`] is never re-resolved in place.**
    ///   Re-resolving it inside a live session would break §3.4's premise 3 and with it the
    ///   patch-equals-a-rebuild equality [`Engine::fragment_for`] rests on. **This is a rule rather
    ///   than a structural impossibility** — it is the property given up to avoid the load spike
    ///   above — so it is the first thing to check in any future change to session handling.
    ///
    /// **Compaction inherits one obligation:** the dictionary length is the monotone counter this
    /// rests on, so a compaction that renumbers the dictionary must not reduce it, or must
    /// introduce a counter that never decreases.
    pub fn is_stale(&self, generation: &Generation) -> bool {
        self.unresolved_count > 0 && generation.dict.len() > self.dict_len_at_authorise
    }
}

/// One (partition, slice)'s live segment count — [`Engine::live_segment_counts`]'s element, and
/// what `/control/status` publishes under `segments`.
///
/// **Plain `String`s and a `usize`, defined here rather than re-exported from `tessera-store`.**
/// `check-layers.sh` denies a `tessera-server → tessera-store` edge (SA §3), so a gauge the server
/// publishes must be nameable from this crate — the discipline `FragmentCacheStats` and
/// `DeclaredScalar` already establish at the crate root. This one owns nothing of the store's
/// vocabulary, so it is a definition here rather than a re-export.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SliceSegments {
    pub partition: String,
    pub slice: String,
    /// Segments this slice's viewport sweep would iterate — base plus every flush extent merge has
    /// not yet collapsed.
    pub segments: usize,
}

/// One partition's live geometry position — [`Engine::partition_status`]'s element, and what
/// `/control/status` publishes as contracts §3.4's per-partition block.
///
/// Defined here rather than re-exported from `tessera-store`, for [`SliceSegments`]' reason: the
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
    Store(StoreError),
    Wal(WalError),
    Plugin(PluginError),
    Io(io::Error),
    /// A viewport request named a slice this bundle doesn't have.
    UnknownSlice(String),
    /// A slice holding a segment whose rows have no known place in the slice's row space.
    ///
    /// `tile_ranges` returns **segment-local** row indices (contracts §2.4) while the mask is a
    /// bitmap over the whole **slice** row space, so serving a segment requires knowing its
    /// `row_base`. Exactly one segment — the build segment, the one `permutation.bin` addresses —
    /// legitimately has no extent and begins at 0; every other arrives with one, from a flush or
    /// from a merge. A second segment with no extent means the row space and the segment list
    /// disagree about what the slice holds.
    ///
    /// **Fails closed because the wrong answer is quiet.** Defaulting such a segment to `row_base
    /// 0` would count its rows against the base segment's mask positions and gather points from
    /// one entity under another's identity — every count plausible, every mark wrong, no error
    /// anywhere. That is a worse outcome than a 500.
    SegmentWithoutRowBase {
        slice: String,
        seg_id: String,
    },
    /// This generation's deny mask has no entry for a slice its bundle carries.
    ///
    /// **Fails closed for the same reason [`Self::SegmentWithoutRowBase`] does: the wrong answer
    /// is silent.** `compose::derive_denied` gives every slice an entry, empty when nothing is
    /// denied, precisely so that a missing one cannot be read as "nothing is denied here". Reading
    /// it that way would compose a mask with the deny half simply absent — every suppressed and
    /// deleted row served on the map, every count including them, and no error anywhere. A 500 is
    /// the better outcome.
    ///
    /// Unreachable while the mask and the bundle are built together, which `Executor::publish`
    /// asserts in debug.
    DenyMaskMissing {
        slice: String,
    },
    /// A slice carried by more than one partition.
    ///
    /// The symmetric case to [`Self::SegmentWithoutRowBase`], and it fails closed for the symmetric
    /// reason: `Engine::viewport` resolves a slice by taking the first partition that carries the
    /// id, and θ's anchor plus every rank is then computed over **that partition alone**. Design
    /// §12.3 requires the anchor to be session-global across partitions — a per-partition anchor
    /// makes "below the cut" mean different things in different partitions, so the coordinator's
    /// union stops computing §7.2's definition. The build emits exactly one partition, so this
    /// is unreachable today; serving a §12 bundle half-masked with no error is what it prevents.
    MultiPartitionSlice(String),
    /// A bundle-level file (`CURRENT`, a plugin hash) was not the shape this engine expects.
    Malformed(String),
    /// `/v1/categories` was asked for a `listing = "per_viewer"` column whose per-`(column, code)`
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
    /// This session's row projection for `(token_id, slice, segments_version)` was being built by
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
            EngineError::Store(e) => write!(f, "store error: {e}"),
            EngineError::Wal(e) => write!(f, "wal error: {e}"),
            EngineError::Plugin(e) => write!(f, "plugin error: {e}"),
            EngineError::Io(e) => write!(f, "io error: {e}"),
            EngineError::UnknownSlice(slice) => write!(f, "unknown slice '{slice}'"),
            EngineError::SegmentWithoutRowBase { slice, seg_id } => write!(
                f,
                "slice '{slice}' holds segment '{seg_id}', which has no extent and so no known \
                 row_base — the row space and the segment list disagree about what this slice \
                 holds (see EngineError::SegmentWithoutRowBase's doc)"
            ),
            EngineError::DenyMaskMissing { slice } => write!(
                f,
                "this generation's deny mask has no entry for slice '{slice}', so the mask and \
                 the bundle disagree about what it holds (see EngineError::DenyMaskMissing's doc)"
            ),
            EngineError::MultiPartitionSlice(slice) => write!(
                f,
                "slice '{slice}' is carried by more than one partition, which this engine's \
                 single-anchor selection does not yet support (see \
                 EngineError::MultiPartitionSlice's doc)"
            ),
            EngineError::Malformed(detail) => write!(f, "malformed: {detail}"),
            EngineError::VocabularyVisibilityUnavailable { column, detail } => write!(
                f,
                "column '{column}' declares `listing = \"per_viewer\"`, and its per-viewer value \
                 visibility could not be derived ({detail}). This column's values are refused \
                 rather than published unfiltered, and rather than served empty — an empty value \
                 set is what a principal who may see none of them is told"
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
    pub(crate) config: EngineConfig,
    next_token_id: AtomicU64,
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
    /// How many row projections were built from the whole fragment rather than derived from the
    /// preceding generation's — the observable behind [`Engine::full_projection_builds`].
    ///
    /// **Unconditional, not `bench-timing`-gated**, unlike `StageTimings::row_projection_built`.
    /// The property it makes testable — that a flush does not cost every session a full rebuild —
    /// is a correctness-shaped one for a deployment's latency, and a test that only runs under a
    /// feature flag is a test that does not run.
    pub(crate) full_projection_builds: AtomicU64,
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
        let side_manifest_high_waters: Vec<u64> = bundle
            .partitions
            .values()
            .map(|partition| partition.manifest.entity_id_high_water)
            .collect();
        let (overlay, buffer, write_state) = WritePath::reconstruct(
            wal_path,
            tessera_lifecycle::alloc::allocator_floor(
                bundle.manifest.entity_id_high_water,
                &side_manifest_high_waters,
            ),
            &dict,
            &initial_deny,
            &mut vocabularies,
            // An entity belongs to exactly one slice, so "any slice's row space holds it" is the
            // same question as "its slice's does" — and asking it this way needs no slice lookup,
            // which the buffer would otherwise have to supply before it has been filtered.
            |entity| {
                bundle.partitions.values().any(|partition| {
                    partition
                        .slices
                        .values()
                        .any(|slice| slice.row_space.row_of(entity).is_some())
                })
            },
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
        let pool = Arc::new(
            rayon::ThreadPoolBuilder::new()
                .num_threads(config.compute_threads)
                .build()
                .map_err(|e| EngineError::ThreadPoolBuild(e.to_string()))?,
        );

        // `Arc`-wrapped from the start: the `Engine` and its `WritePath` share this one
        // pointer, so a swap published by an acceptance is the swap every read path observes.
        // The mask this engine opens with, derived from the overlay replay reconstructed and the
        // row space the bundle carries — the same derivation every later publication repeats
        // (`compose::derive_denied`). A node restarting into a live suppression set gets it here,
        // not on its first request.
        let bundle = Arc::new(bundle);
        let denied = Arc::new(crate::compose::derive_denied(&overlay, &bundle));

        // The filter artefact belongs to the published prefix, so it is opened here with the
        // bundle and carried forward by every generation successor. The build's column plus every
        // extent the partition's side-manifest names: a restart therefore composes exactly what the
        // flushes before it published, rather than serving the build's coverage and answering short
        // over everything ingested since (`filter-index.md` §2.1).
        let filter_columns = {
            let partition = bundle.partitions.keys().next().cloned().unwrap_or_default();
            let extents = bundle
                .partitions
                .get(&partition)
                .map(|p| p.manifest.attr_extents.clone())
                .unwrap_or_default();
            // The record blob rides the same open (records §3, §7): the base the schema owes plus
            // every extent the side-manifest names — always the full shape, even while no flush
            // writes one, so a restart composes whatever was published.
            let record_extents = bundle
                .partitions
                .get(&partition)
                .map(|p| p.manifest.record_extents.clone())
                .unwrap_or_default();
            let text_extents = bundle
                .partitions
                .get(&partition)
                .map(|p| p.manifest.text_extents.clone())
                .unwrap_or_default();
            Arc::new(
                crate::filter::FilterColumns::open(
                    &prefix_dir,
                    &partition,
                    &bundle.manifest.declared_scalars,
                    &bundle.manifest.vocabularies,
                    &extents,
                    &record_extents,
                    &text_extents,
                    // Mapped, for the reason `FilterColumns::open` gives: the engine opens every
                    // declared column at once and holds them for the process lifetime, so the
                    // alternative is tens of GB of residency at 10⁹ paid before any filter arrives.
                    true,
                )
                .map_err(|e| {
                    EngineError::Store(tessera_store::StoreError::Io {
                        path: prefix_dir.join("partitions").join(&partition),
                        source: e,
                    })
                })?,
            )
        };

        let generation = Arc::new(ArcSwap::new(Arc::new(Generation {
            prefix,
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
            denied,
        })));

        // Unbounded until `set_cache_bounds` is called. `tessera-server` calls it immediately
        // after `open`, having validated the figure; every other embedder (tests, benches,
        // examples) gets unbounded caches, which is what a read-only embedder wants.
        let row_projection_cache = Arc::new(RowProjectionCache::new(u64::MAX));
        let refresh_in_flight = Arc::new(AtomicU64::new(crate::refresh::NO_REFRESH));
        let refresh_enabled = Arc::new(AtomicBool::new(true));
        let refresh_paused = Arc::new(AtomicBool::new(false));
        let coalesce_enabled = Arc::new(AtomicBool::new(true));
        let merge_enabled = Arc::new(AtomicBool::new(true));
        let fold_paused = Arc::new(AtomicBool::new(false));
        let fold_publication_paused = Arc::new(AtomicBool::new(false));

        Ok(Engine {
            generation: Arc::clone(&generation),
            plugin,
            // Unbounded until `set_cache_bounds` is called. `tessera-server` calls it immediately
            // after `open`, having validated the figure; every other embedder (tests, benches,
            // examples) gets unbounded caches, which is what a read-only embedder wants.
            row_projection_cache: Arc::clone(&row_projection_cache),
            pool,
            bundle_root: bundle_root.to_path_buf(),
            config,
            next_token_id: AtomicU64::new(0),
            write: WritePath::new(write_state),
            identity_key,
            boot_nonce: OsRng.next_u64(),
            serial_fallback_max_rows: AtomicU64::new(crate::viewport::SERIAL_FALLBACK_MAX_ROWS),
            filter_crossings_projected: AtomicU64::new(0),
            filter_crossings_per_tile: AtomicU64::new(0),
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
            full_projection_builds: AtomicU64::new(0),
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
    /// Turn the background refresh off, so a session stays in the stale-serve window.
    ///
    /// **A test hook, and gated so it cannot exist in a shipped build.** The window decision
    /// 0044's rung 2 serves from is otherwise a race between the publication and the pool: a test
    /// that slept to catch it would assert on scheduling. `fault-injection` is the gate the
    /// integration suites already enable, and `scripts/check-layers.sh` asserts no normal
    /// dependency edge does.
    #[cfg(feature = "fault-injection")]
    #[doc(hidden)]
    pub fn set_background_refresh_for_test(&self, enabled: bool) {
        self.refresh_enabled.store(enabled, Ordering::SeqCst);
    }

    /// Hold the background refresh, leaving it **in flight** — the window rung 3 of
    /// `Engine::session_geometry`'s ladder sheds a racer in. Distinct from
    /// [`Self::set_background_refresh_for_test`], which models a refresh that produces nothing and
    /// *finishes*: the flag clears there, and rung 3 builds instead of refusing.
    #[cfg(feature = "fault-injection")]
    #[doc(hidden)]
    /// Hold a fold between its last pass and its submission, so a test can land a flush **inside
    /// the fold's flight** — the interleaving compaction §2's whole carry-forward table exists for,
    /// and the one nothing else here can construct.
    ///
    /// A fold over a fixture-sized corpus finishes in well under a second, so an unassisted test
    /// racing a flush against it would be a coin toss; over a real corpus the same window is hours
    /// wide and needs no help. What the hook models is therefore the *duration*, not a behaviour:
    /// the flush publishes into the old prefix exactly as it would in production, and the
    /// publication that follows sees it as a carry-forward through the live manifest, which is the
    /// only route it ever has.
    ///
    /// Held after the passes rather than during them, which is where the interleaving's
    /// consequences live: a flush landing mid-pass is one the fold's file list does not name, and
    /// so is one the publication must carry forward — which is the same state this produces.
    pub fn set_fold_paused_for_test(&self, paused: bool) {
        self.fold_paused.store(paused, Ordering::SeqCst);
    }

    /// Hold a **completed** fold in its channel, undrained, so a merge or coalesce can publish
    /// under it — see [`crate::write::MaintenanceDeps::fold_publication_paused`] for which window
    /// this is and why it is a real one.
    ///
    /// Unpausing wakes the executor, because the loop that would drain the fold may already have
    /// parked: the pause makes `publish_completed_folds` report that nothing happened, which is
    /// exactly what sends an otherwise-idle executor to `wait_for_work`.
    pub fn set_fold_publication_paused_for_test(&self, paused: bool) {
        self.fold_publication_paused.store(paused, Ordering::SeqCst);
        if !paused {
            self.write.wake();
        }
    }

    /// Whether a fold has finished its passes and is holding at [`Self::set_fold_paused_for_test`].
    /// The condition a test waits on instead of guessing at the hold with a sleep.
    pub fn fold_is_holding_for_test(&self) -> bool {
        self.write.health().fold_holding.load(Ordering::SeqCst)
    }

    pub fn set_refresh_paused_for_test(&self, paused: bool) {
        self.refresh_paused.store(paused, Ordering::SeqCst);
    }

    /// Turn the row-space merge off — see [`Self::merge_enabled`]. Same gate, same reasoning as
    /// [`Self::set_background_refresh_for_test`].
    #[cfg(feature = "fault-injection")]
    #[doc(hidden)]
    pub fn set_merge_for_test(&self, enabled: bool) {
        self.merge_enabled.store(enabled, Ordering::SeqCst);
    }

    /// Turn the entity-space coalesce off. The soak's control needs both passes stopped, to show
    /// the axes it bounds do grow — a bound assertion against a policy that never triggers is
    /// indistinguishable from one against a policy that works.
    #[cfg(feature = "fault-injection")]
    #[doc(hidden)]
    pub fn set_coalesce_for_test(&self, enabled: bool) {
        self.coalesce_enabled.store(enabled, Ordering::SeqCst);
    }

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

        // Loaded once, here, and used for both the dictionary and the watermark below — the
        // ordering invariant lifecycle §1.1 states: a request resolves everything against one
        // generation, or it can resolve `satisfied` against a dictionary a later flush published
        // while building a fragment against the watermark that preceded it.
        let generation = self.generation.load();

        // Counted rather than derived as `terms.len() - satisfied.len()`: `satisfied` is a set, so
        // two descriptors resolving to one ordinal would make that difference report an unresolved
        // descriptor that does not exist. Only `> 0` is ever read (`Session::is_stale`), but a
        // count that can be wrong for a reason unrelated to the dictionary is not one to keep.
        let mut satisfied: FxHashSet<TermId> = FxHashSet::default();
        let mut unresolved_count = 0usize;
        for descriptor in &auth_terms.terms {
            match generation.dict.lookup(descriptor) {
                Some(term) => {
                    satisfied.insert(term);
                }
                // An unknown descriptor is simply unsatisfied, never an error — and §3.3's
                // observation is that the ones that drop out here are precisely this session's
                // exposure to a later promotion, so the condition costs a counter to keep and a
                // rebuild to recover.
                None => unresolved_count += 1,
            }
        }

        let mut satisfied_sorted: Vec<TermId> = satisfied.iter().copied().collect();
        satisfied_sorted.sort_unstable();
        let satisfied_sorted = Arc::new(satisfied_sorted);

        // The cache's caller obligation (`FragmentCache::get_or_build`'s doc): this hash must be
        // a function of the exact `auth_data` that produced `satisfied` above, which it is.
        let auth_data_hash: [u8; 32] = Sha256::digest(auth_data).into();

        let fragment = generation
            .fragments
            .get_or_build(
                &satisfied_sorted,
                auth_data_hash,
                generation.segments_version,
                &generation.postings,
                &generation.delta_postings,
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
            satisfied_sorted,
            auth_data_hash,
            expires_at,
            unresolved_count,
            dict_len_at_authorise: generation.dict.len(),
            segments_version_at_authorise: generation.segments_version,
        })
    }

    /// Delegates to `WritePath::allocator_high_water`, which owns the allocator; see that
    /// method's doc.
    pub fn allocator_high_water(&self) -> u64 {
        self.write.allocator_high_water()
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

    /// Publish a **new prefix** this process just wrote: open it, rotate the term index, the
    /// bundle identity, the fragment cache and the external-id sidecar onto it, retire `retired`,
    /// and swap — steps 5 and 6 of compaction §4, as one call.
    ///
    /// # There is no production caller, and the name now says so
    ///
    /// **The fold does not take this route.** It publishes from the executor thread, where a
    /// submission to the executor would deadlock, so it calls [`open_rotation`] — the seam both
    /// share — and publishes inline (`crate::write::Executor::publish_fold`). Nothing else writes a
    /// prefix. This entry point existed for "an embedder that wrote a prefix by some other means",
    /// which is a caller that does not exist, and its only real users are the prefix-rotation cases
    /// in `tests/prefix_rotation.rs`, which exercise the swap's half of compaction §4 against a
    /// stand-in prefix rather than a whole fold.
    ///
    /// That mattered because of what it accepts. `retired` is Rule F's executed set and **this
    /// function retires whatever it is handed** — the one place compaction §5's derivation is
    /// enforced by documentation rather than by construction, since a caller could pass any bitmap
    /// and make a deletion's tombstone leave `deleted` while its item is still visible. Keeping a
    /// `pub` name that reads like the production route, in front of that, is an invitation. The
    /// seam stays covered; what changes is that nobody reaches for this by accident.
    ///
    /// The refusals, the identity, and why the open skips verification are all [`open_rotation`]'s
    /// and documented there.
    ///
    /// # `watermark` and `dict` are the **live** values, passed through untouched
    ///
    /// Compaction §4 step 2 and pass 4. A fold folds rows; it does not advance the entity axis and
    /// it does not renumber the dictionary, so both come from the live generation and neither is
    /// derived from the fold's inputs — deriving the watermark that way moves it backwards past
    /// every entity accepted since the fold's snapshot, and every one of them goes invisible.
    /// `crate::geometry::check_publishable` refuses a regression rather than trusting this
    /// paragraph.
    ///
    /// # `retired` — Rule F, and the caller's obligation
    ///
    /// Entities whose tombstones leave `deleted` in this same swap. **Only those whose row and
    /// postings this publication demonstrably removed** — compaction §5's
    /// `{ e ∈ D₀ : no carried-forward artefact names e }`, evaluated against what was published
    /// and never against what the plan predicted, with *artefact* meaning tier, segment **and**
    /// external-id run. See `tessera_lifecycle::Overlay::retire`, which states what retiring one
    /// entity too many costs. Empty is always safe: an un-retired tombstone is fail-closed, and
    /// the next fold takes it.
    pub fn publish_rotated_prefix_for_test(
        &self,
        prefix: &str,
        segments_version: u64,
        watermark: u64,
        dict: Arc<Dict>,
        delta_postings: Vec<Arc<DeltaTier>>,
        retired: &[EntityId],
    ) -> std::result::Result<(), PublishGeometryError> {
        let mut retired_bitmap = croaring::Bitmap::new();
        for entity in retired {
            retired_bitmap.add(u32::try_from(entity.raw()).expect(
                "entity ids are capped at u32::MAX by the I9 allocator (contracts §2.6 r6)",
            ));
        }
        let (bundle, rotation) = open_rotation(
            &self.bundle_root,
            prefix,
            &self.generation.load().fragments,
            retired_bitmap,
        )?;

        self.write.publish_geometry(
            GeometryPublication::within_prefix(
                prefix.to_string(),
                segments_version,
                watermark,
                bundle,
                dict,
                delta_postings,
            )
            .rotating(rotation),
        )
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
        self.generation
            .load()
            .fragments
            .set_memory_bound(fragment_bytes);
    }

    /// How long a request parks on another request's in-flight row-projection build before it is
    /// refused (`serve.single_flight_wait_ms`, decision 0058).
    ///
    /// A setter for [`Self::set_cache_bounds`]'s reason and by its route: `EngineConfig` is
    /// exhaustively constructed at fifteen sites, three of them in frozen test files.
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

    /// Live segments per (partition, slice), read straight off the current generation — the gauge
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
    /// Sorted by `(partition, slice)` because the generation holds them in `HashMap`s: an operator
    /// diffing two status responses must not see a reordering that means nothing.
    pub fn live_segment_counts(&self) -> Vec<SliceSegments> {
        let generation = self.generation.load();
        let mut counts: Vec<SliceSegments> = generation
            .bundle
            .partitions
            .iter()
            .flat_map(|(partition, data)| {
                data.slices
                    .iter()
                    .map(move |(slice, slice_data)| SliceSegments {
                        partition: partition.clone(),
                        slice: slice.clone(),
                        segments: slice_data.segments.len(),
                    })
            })
            .collect();
        counts.sort_by(|a, b| (&a.partition, &a.slice).cmp(&(&b.partition, &b.slice)));
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

    /// `session`'s mask fragment **at `generation`'s watermark** — the one thing on the request
    /// path that may stand in for `Session::fragment`.
    ///
    /// # Why a session's fragment cannot simply be the one it authorised with
    ///
    /// A fragment is materialised once per session (I2) and frozen. A flush then publishes a delta
    /// postings tier and advances the watermark, and `compose`'s rule 4 admits buffered entities at
    /// or above **the fragment's own** watermark — so an entity that a flush moved out of the
    /// buffer and into a tier falls between the two: no longer buffered, not yet in this fragment.
    /// It is invisible to that session until it re-authorises. Not fail-open — the item is missing,
    /// not wrongly shown — but it is the very property the flush exists to deliver, silently
    /// undone for exactly the sessions that were open when it happened.
    ///
    /// # Why this is a rebuild and not §11.2's patch, stated rather than glossed
    ///
    /// Design §11.2 specifies advancing the fragment by OR-ing in the flushed segment's
    /// contribution for the session's already-satisfied terms — *"a small, monotone patch rather
    /// than a rebuild"* — and write-path §4.6 sets out the four premises under which that patch is
    /// **equal** to what a rebuild produces. **This is the rebuild.** It is correct for the same
    /// reason the patch would be: `satisfied` is fixed at authorise and never re-resolved (premise
    /// 3), so the terms unioned are exactly the terms a rebuild consults.
    ///
    /// **⊘ The incremental form is not built, and is not being built** — a ruling, not a backlog
    /// entry. Decision 0044's D4 made it conditional on this being seconds-scale at 10⁹; probe P2
    /// measured **~200 ms and flat in tier count**, refuting the model. The incremental form would
    /// trade that for a ~41 ms bitmap clone, on work that had to move off the request thread
    /// anyway — and the mechanism that moved it (`crate::refresh`) is the same one the projection
    /// needed. Re-open it if a credential's satisfied set grows by orders, the base union being
    /// the whole of the 200 ms.
    ///
    /// **What that costs, and where it is now paid.** One `build_fragment_with_deltas` per
    /// *credential*, not per session: [`tessera_authz::FragmentCache`] keys on
    /// `(satisfied, auth_data_hash, resolved_at, watermark)`, so every session sharing a credential
    /// shares the build. **Measured at ~200 ms at 10⁹ and flat in tier count**
    /// (`probes/2026-08-04-refresh-ladder/` — P2, which refuted the modelled-seconds figure the
    /// corpus carried). That is three orders over decision 0044's request-path budget, so this no
    /// longer runs per tick on a request thread: the background refresh (`crate::refresh`) calls
    /// it at each publication, and a request reaches it only at establishment — rung 3 of
    /// `Engine::session_geometry`'s ladder, and `Engine::item` when this session has no resident
    /// entry at all.
    ///
    /// **Fail-closed on a busy build**: a concurrent build of the same key yields
    /// [`EngineError::FragmentBuilding`] (429) rather than a silent fall back to the stale
    /// fragment. Serving the stale one would be the quiet wrong answer this method exists to
    /// remove.
    pub(crate) fn fragment_for(
        &self,
        session: &Session,
        generation: &Generation,
    ) -> Result<Arc<FrozenFragment>> {
        // **Both tests, and the identity one is not redundant.** A flush advances the watermark, so
        // the watermark alone decides whether a session's own fragment is still current *within* a
        // prefix. A fold advances no watermark at all — it rewrites the term index and rotates the
        // bundle identity (decision 0050) — so on the watermark test alone a session authorised
        // before a fold would go on composing against a fragment that still contains every entity
        // the fold retired. That is Rule F re-exposing exactly what it withdrew, and it is why
        // compaction §4 puts the comparison *here*, at composition: this fragment is held by
        // `Session`, outside `FragmentCache` altogether, so rotating the cache does not reach it.
        if session.fragment.identity == generation.bundle_identity()
            && session.fragment.watermark >= generation.watermark
        {
            return Ok(Arc::clone(&session.fragment));
        }
        generation
            .fragments
            .get_or_build(
                &session.satisfied_sorted,
                session.auth_data_hash,
                // **The generation the session's `satisfied` was resolved against, not the live
                // one** (#112). `get_or_build` memoises `auth_data_hash → canonical key` under
                // `(hash, resolved_at, watermark)`, and its caller obligation is that the hash and
                // the stamp "must never arrive paired with two different term sets". This session's
                // `satisfied` was frozen at authorise, against the generation as it stood then;
                // passing the *live* stamp pairs a stale term set with a generation that has moved
                // past it, and the memo keeps that pairing.
                //
                // The cost was not to this session, which is stale either way until it
                // re-authorises (`Session::is_stale`). It was to the **next** authorise of the same
                // credential: that one resolves the promoted descriptor correctly, hits the entry
                // this call left behind, and is handed the fragment for the grant set it has just
                // stopped having — a session served its pre-flush visible set indefinitely, with
                // nothing later repairing it. Same bytes took the poisoned path and different bytes
                // naming the same terms did not, which is the asymmetry that identified it.
                session.segments_version_at_authorise,
                &generation.postings,
                &generation.delta_postings,
                generation.watermark,
            )
            .map_err(|e| match e {
                FragmentCacheError::Building => EngineError::FragmentBuilding,
                FragmentCacheError::Io(io_err) => EngineError::Io(io_err),
            })
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

    /// Whether the external-id sidecar has opened any extent (or its locator) yet — exposed for
    /// tests confirming `Engine::open` never touches it — the per-extent laziness guarantee.
    pub fn external_id_sidecar_is_open(&self) -> bool {
        self.generation.load().external_index.0.is_open()
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
        self.write
            .resolve_terms(&self.generation.load().dict, descriptors)
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
        self.generation.load().external_index.resolve(external_id)
    }

    /// Batch form of [`Self::resolve_external_id`] for `/control/ingest`'s duplicate check
    /// (contracts §3.1 r6): live map first for the *whole* batch (Important I-8 — `established`
    /// holds every id ingested since the build, which the sidecar cannot see at all, and is
    /// exactly where a retried client batch's duplicate lives), then one batched, sorted sidecar
    /// call for whatever residual keys the live map didn't resolve — each bundle extent is opened
    /// at most once regardless of batch size, never once per row.
    ///
    /// Returns one `Option<EntityId>` per input, in the caller's given order.
    /// Invert `tessera_id`s to entity ids for the admin plane, all-or-nothing.
    ///
    /// **The idset is checked first, against the same generation the inversions use** — one
    /// `load_full`, exactly as [`crate::viewport::Engine::item`] does it, so a swap landing
    /// mid-call cannot validate the idset against one snapshot and invert under another. A
    /// mismatch is [`EngineError::StaleIdSet`] and **decides before any inversion happens**: a
    /// caller holding a list gathered before a key rotation is refused wholesale rather than
    /// having its identifiers reinterpreted under the new key, which would name different live
    /// items (decision 0025).
    ///
    /// **`None` for an identifier that names nothing**, per position, so a caller learns which.
    /// The permutation is total — every `u64` inverts to *something* — so the range check is the
    /// whole of the misdirection guard: a shard that is not this one, or an entity at or above the
    /// allocator's high-water, cannot name an item this deployment ever issued. Both are facts
    /// about the identifier space rather than about any item's visibility, and this is the admin
    /// plane (R5), so refusing precisely discloses nothing a caller could not compute.
    ///
    /// Ordered as the caller supplied, like [`Self::resolve_external_ids`], so a refusal can name
    /// the offending position.
    pub fn resolve_tessera_ids(
        &self,
        ids: &[TesseraId],
        idset: u32,
    ) -> Result<Vec<Option<EntityId>>> {
        let generation = self.generation.load_full();
        if idset != generation.bundle.manifest.identity.idset {
            return Err(EngineError::StaleIdSet);
        }
        let shard = generation.bundle.manifest.identity.shard_id;
        let high_water = self.allocator_high_water();
        Ok(ids
            .iter()
            .map(|id| {
                let (id_shard, entity) = self.identity_key.invert(*id);
                (id_shard == shard && entity.raw() < high_water).then_some(entity)
            })
            .collect())
    }

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
        let residual_results = self
            .generation
            .load()
            .external_index
            .resolve_many(&residual_keys)?;
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
        self.external_id_of_in(&self.generation.load(), entity)
    }

    /// [`Self::external_id_of`] against a generation the caller already loaded.
    ///
    /// `Engine::item` is the caller, and it must not take a second `load()`: the sidecar is now
    /// per-generation (a fold rewrites it — see [`Generation::external_index`]), so a drill-down
    /// that resolved its row against one generation and its external id against another would be
    /// exactly the cross-generation mix I11's within-request rule forbids, reached through the one
    /// field that used to be process-wide.
    pub(crate) fn external_id_of_in(
        &self,
        generation: &Generation,
        entity: EntityId,
    ) -> std::result::Result<Option<Vec<u8>>, StoreError> {
        if let Some(external_id) = self.write.established_external_id(entity) {
            return Ok(Some(external_id));
        }
        generation
            .external_index
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
            Arc::clone(&self.row_projection_cache),
            queue_bound,
            crate::write::MaintenanceDeps {
                max_age_secs: self.config.flush_max_age_secs,
                coalesce: crate::coalesce::CoalescePolicy::default(),
                merge: merge_policy(self.config.max_merged_segment_bytes),
                // The **configured** value, not the resolved policy's: compaction §4 step 3
                // re-checks write-path §7's base-segment relation against the fold's own output,
                // and `tessera-server`'s loader checks only an explicitly set one.
                configured_merge_bytes: self.config.max_merged_segment_bytes,
                compaction: self.config.compaction,
                coalesce_enabled: Arc::clone(&self.coalesce_enabled),
                merge_enabled: Arc::clone(&self.merge_enabled),
                fold_paused: Arc::clone(&self.fold_paused),
                fold_publication_paused: Arc::clone(&self.fold_publication_paused),
                refresh: crate::refresh::RefreshDeps {
                    cache: Arc::clone(&self.row_projection_cache),
                    pool: Arc::clone(&self.pool),
                    in_flight: Arc::clone(&self.refresh_in_flight),
                    refreshes: Arc::clone(&self.refreshes),
                    enabled: Arc::clone(&self.refresh_enabled),
                    paused: Arc::clone(&self.refresh_paused),
                },
                bundle_root: self.bundle_root.clone(),
                identity_key: self.identity_key,
                pool: Arc::clone(&self.pool),
                max_distinct_terms: self.plugin.declared_bounds().max_distinct_terms,
                // Above every candidate any partition carries, so a manifest stepped past for
                // failing verification is never overwritten.
                next_manifest_n: self
                    .generation
                    .load()
                    .bundle
                    .partitions
                    .values()
                    .map(|p| p.highest_candidate_n + 1)
                    .max()
                    .unwrap_or(1),
            },
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
        self.write.start_executor(
            generation,
            Arc::clone(&self.row_projection_cache),
            queue_bound,
            crate::write::MaintenanceDeps {
                max_age_secs: self.config.flush_max_age_secs,
                coalesce: crate::coalesce::CoalescePolicy::default(),
                merge: merge_policy(self.config.max_merged_segment_bytes),
                // The **configured** value, not the resolved policy's: compaction §4 step 3
                // re-checks write-path §7's base-segment relation against the fold's own output,
                // and `tessera-server`'s loader checks only an explicitly set one.
                configured_merge_bytes: self.config.max_merged_segment_bytes,
                compaction: self.config.compaction,
                coalesce_enabled: Arc::clone(&self.coalesce_enabled),
                merge_enabled: Arc::clone(&self.merge_enabled),
                fold_paused: Arc::clone(&self.fold_paused),
                fold_publication_paused: Arc::clone(&self.fold_publication_paused),
                refresh: crate::refresh::RefreshDeps {
                    cache: Arc::clone(&self.row_projection_cache),
                    pool: Arc::clone(&self.pool),
                    in_flight: Arc::clone(&self.refresh_in_flight),
                    refreshes: Arc::clone(&self.refreshes),
                    enabled: Arc::clone(&self.refresh_enabled),
                    paused: Arc::clone(&self.refresh_paused),
                },
                bundle_root: self.bundle_root.clone(),
                identity_key: self.identity_key,
                pool: Arc::clone(&self.pool),
                max_distinct_terms: self.plugin.declared_bounds().max_distinct_terms,
                // Above every candidate any partition carries, so a manifest stepped past for
                // failing verification is never overwritten.
                next_manifest_n: self
                    .generation
                    .load()
                    .bundle
                    .partitions
                    .values()
                    .map(|p| p.highest_candidate_n + 1)
                    .max()
                    .unwrap_or(1),
            },
            Some(faults),
        )
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

    /// The last compaction fold's per-pass wall clock and resident set — empty before the first
    /// fold. Operator plane only, beside [`Engine::write_executor_stats`].
    pub fn last_fold_passes(&self) -> Vec<crate::compact::PassCost> {
        self.write.health().last_fold_passes()
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

    /// Request a flush. **Accepted at any time, executed promptly** (contracts §3.4): the flag
    /// pulls the tick's deadline forward and the doorbell wakes an idle executor, so the flush
    /// runs at the next loop iteration — through the one tick path, with everything a tick
    /// guarantees. The 202 still means "accepted, not yet done": the segment write is pool work
    /// of real duration, and the response never waits on it.
    ///
    /// This is an *operator* trigger and deliberately the only thing that may pull the tick: it
    /// is rate-decoupled from ingest, so it cannot recreate the publish-on-trip hazard that got
    /// `flush_max_items` deleted (decision 0045) — a publication period proportional to load,
    /// rotating every session's projection key at that rate.
    pub fn request_flush(&self) {
        self.write
            .health()
            .flush_requested
            .store(true, Ordering::SeqCst);
        self.write.wake();
    }

    /// Request a **compaction fold** — the trigger `POST /control/compact` will take (contracts
    /// §3.4, reserved and unbuilt).
    ///
    /// The same shape as [`Engine::request_flush`] and for the same reason: the flag is read at the
    /// tick, on the one thread that publishes, so everything a tick guarantees holds for a
    /// requested fold. What differs is where the work runs — a fold gets its own thread rather than
    /// the shared pool (compaction §3) — and how long it takes: a 202 here means "accepted", and
    /// the fold is minutes to hours of IO afterwards.
    ///
    /// **At most one fold is in flight and a second request is not queued**: a fold requested while
    /// one runs is satisfied by neither, because the flag is a flag. That is the same answer
    /// compaction §9 gives the automatic trigger, which is refused outright while one runs.
    ///
    /// ⊘ **The trigger's three gauges and its minimum interval are not built** (compaction §9). A
    /// fold happens when something calls this, which for now is an operator or a test.
    pub fn request_fold(&self) {
        self.write
            .health()
            .fold_requested
            .store(true, Ordering::SeqCst);
        self.write.wake();
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
        // **Every buffered row has a cell**, established here because this is the boundary rows
        // enter the buffer through — and it has more than one caller. A check in the HTTP handler
        // guarded one of them and left the bench arms, the tests and any future ingest route
        // writing points the quantiser would silently clamp onto the edge of the grid.
        //
        // Before the submit, so an out-of-extent row is refused with nothing acked, nothing
        // WAL-durable and no entity id burned (I9). `plan_flush` is entitled to assume this and
        // does; a second copy of the predicate there is how the two would come to disagree.
        // **Step-down gates ingest** (owner-ruled 2026-08-04; write-path §5.6). Before the
        // per-row checks: this is node state, not row state, and refusing here — the boundary
        // with more than one caller — is what keeps a stepped-down node from accepting rows a
        // flush would then bury under a manifest assembled from the older served state. Denies
        // are deliberately not gated; see `AcceptError::SteppedDown`.
        if self.any_partition_stepped_down() {
            return Err(crate::write::AcceptError::SteppedDown);
        }
        // **Arity before the submit.** The commit window indexes `row.scalars` positionally against
        // `declared_scalars` to find a category key's vocabulary, so a short row indexed out of
        // bounds and panicked the executor — surfacing as a lost receipt rather than a refusal.
        // Checked here for the same reason the extent check is: the invariant is about the buffer,
        // and the buffer has more than one writer.
        let declared = self.meta().declared_scalars.len();
        if let Some((index, row)) = rows
            .iter()
            .enumerate()
            .find(|(_, row)| row.scalars.len() != declared)
        {
            return Err(crate::write::AcceptError::ScalarArity {
                index,
                expected: declared,
                got: row.scalars.len(),
            });
        }
        let quantisation = self.meta().quantisation;
        if let Some((index, row)) = rows
            .iter()
            .enumerate()
            .find(|(_, row)| !quantisation.contains(row.x, row.y))
        {
            return Err(crate::write::AcceptError::OutsideExtent {
                index,
                x: row.x,
                y: row.y,
                quantisation,
            });
        }
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
        entity: EntityId,
        op: ChangeOp,
    ) -> std::result::Result<(), crate::write::AcceptError> {
        self.write.accept_change(entity, op)
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
        entity: EntityId,
        op: ChangeOp,
    ) -> std::result::Result<crate::write::PendingChange, crate::write::AcceptError> {
        self.write.submit_change(entity, op)
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
    let bundle = Arc::new(
        tessera_store::open_written_prefix(bundle_root, prefix)
            .map_err(|e| PublishGeometryError::PrefixNotOpenable(e.to_string()))?,
    );
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
    let filter_columns = Arc::new(
        crate::filter::FilterColumns::open(
            &prefix_dir,
            &phash,
            &bundle.manifest.declared_scalars,
            &bundle.manifest.vocabularies,
            &partition.manifest.attr_extents,
            // The record blob rotates with the prefix for the reason the value columns do: the
            // fold rewrites it, and the superseded prefix's files are pre-blanking.
            &partition.manifest.record_extents,
            &partition.manifest.text_extents,
            true,
        )
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
pub(crate) struct ExternalIdIndex(tessera_store::ExternalIdSidecar);

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
