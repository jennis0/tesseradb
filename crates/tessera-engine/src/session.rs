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
use rustc_hash::{FxHashMap, FxHashSet};
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
    /// The flush's **row** trigger: buffered rows at which the tick comes due early.
    ///
    /// The age tick alone leaves `B` — rows buffered between publications — as the arrival rate
    /// times the period, which is unbounded in the rate. Every commit-window close deep-copies the
    /// buffer, so a window costs `O(B)` and a flush interval pays `B²/2W`
    /// (`docs/evidence/memos/2026-08-05-ingest-rate.md`). This is what bounds `B`, and with it the
    /// per-row cost of a fast loader; `flush_max_age_secs` still bounds how *stale* a slow one's
    /// rows may be. A publication satisfies both, so the two triggers never compound.
    pub flush_max_items: usize,
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
    /// `serve.tier_width` — how many adjacent, same-tier segments select a row-space merge
    /// (write-path §7). **`None` keeps the built-in default (4)**; see [`merge_policy`].
    ///
    /// This key shared `max_merged_segment_bytes`' defect and is wired for its reason: the server
    /// parsed and validated it and it reached the engine nowhere, so `merge_policy` hard-coded 4
    /// whatever an operator set. Now it arrives here.
    ///
    /// **Must be at least 2 when set.** `MergePolicy::select` returns `None` below 2, so a width
    /// of 1 is not "merge eagerly" but "never merge" — segments accumulate for ever and the
    /// failure looks like a policy that is simply never triggered. `tessera-server`'s loader
    /// refuses widths below 2 at startup rather than letting the first tick discover them
    /// silently; an embedder constructing this struct directly is on its own honour, which is why
    /// the constraint is stated here as well as in the loader.
    pub tier_width: Option<usize>,
    /// `serve.segment_floor_bytes` — sizes at or below this compare equal for merge selection, so
    /// a tail of tiny flush segments forms one tier rather than a ladder of singletons that never
    /// reaches `tier_width` (write-path §7). **`None` keeps the built-in default (16 MiB)**; see
    /// [`merge_policy`]. Wired for [`Self::tier_width`]'s reason: parsed by the server, previously
    /// discarded. Any value is usable — `0` simply means sizes compare by their own power-of-two
    /// class — so unlike the widths there is nothing to refuse.
    pub segment_floor_bytes: Option<u64>,
    /// How many same-tier entries, per axis, select an entity-space coalesce (write-path §7).
    /// **`None` keeps the built-in default (8)** — see [`crate::coalesce::CoalescePolicy`], whose
    /// `Default` carries the argument for that number.
    ///
    /// The two merge knobs above were parsed and discarded; this one had no configuration key at
    /// all, so the width was a constant nothing could reach. It gets a key because the correctness
    /// suite has to be able to make a coalesce eligible at a chosen point rather than after eight
    /// flushes (correctness-suite §12.3). **Must be at least 2 when set**, for
    /// [`Self::tier_width`]'s reason verbatim: below 2 the pass is silently disabled, and the
    /// server's loader refuses that at startup.
    pub coalesce_width: Option<usize>,
    /// When a fold is dispatched with nobody asking for one — compaction §9's automatic trigger,
    /// as decision 0056 rules it.
    ///
    /// **[`CompactionSchedule::off`] is the value an embedder gets unless it says otherwise**, and
    /// that is deliberate: a fold is minutes to hours of IO, and starting one from a default nobody
    /// chose is not a decision this type may make on a caller's behalf. `tessera-server` applies
    /// §9's defaults, because it is where an operator can see and change them.
    pub compaction: crate::compact::CompactionSchedule,
}

/// The row-space merge's policy (write-path §7), with every configured knob applied.
///
/// **Defaults: `tier_width` 4, floor 16 MiB, cap 256 MiB.** The floor is where per-segment
/// overheads stop dominating, and clamping to it before taking the size class is what stops a
/// deployment whose flushes differ by a few bytes producing a size class per flush and merging
/// nothing at all. The cap bounds one merge's pool time and its write amplification.
///
/// **All three knobs are read from the config now; the first two used to be hard-coded here.**
/// `serve.tier_width` and `serve.segment_floor_bytes` were parsed and validated by
/// `tessera-server` and reached this function nowhere — the inert-key defect decision 0045
/// forbids, the same one `max_merged_segment_bytes` had until it was wired — so an operator who
/// set either got 4 and 16 MiB with no signal at all. An unset knob (`None`) keeps the default
/// above, so no configuration that never named the keys changes behaviour.
///
/// **The base segment is excluded twice over.** `crate::merge::plan_merge` selects from the
/// **extent list**, and the base is the one segment with no extent (`permutation.bin` addresses
/// it), so no size makes it selectable. Write-path §7's **enforced relation** —
/// `max_merged_segment_bytes` strictly below the base segment's bytes — stands beside that and is
/// still refused at startup by `tessera-server`'s config loader. Both are kept deliberately: the
/// structural exclusion lives in one function and a refactor could lose it, and the startup
/// refusal is what would still be standing if it did.
fn merge_policy(config: &EngineConfig) -> tessera_store::merge::MergePolicy {
    tessera_store::merge::MergePolicy {
        tier_width: config.tier_width.unwrap_or(4),
        segment_floor_bytes: config.segment_floor_bytes.unwrap_or(16 << 20),
        max_merged_segment_bytes: config.max_merged_segment_bytes.unwrap_or(256 << 20),
    }
}

/// The entity-space coalesce's policy, with `EngineConfig::coalesce_width` applied when set.
///
/// Only the width is configurable: it is the knob that decides *when* the pass becomes eligible,
/// which is what the correctness suite has to control (correctness-suite §12.3). The floor and the
/// input cap keep [`crate::coalesce::CoalescePolicy`]'s defaults — nothing reads them from
/// configuration, and a key nothing needs would be minted only to become the next inert one.
fn coalesce_policy(config: &EngineConfig) -> crate::coalesce::CoalescePolicy {
    let mut policy = crate::coalesce::CoalescePolicy::default();
    if let Some(width) = config.coalesce_width {
        policy.width = width;
    }
    policy
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
    /// row-projection cache key (`(token_id, view, segments_version)`, shared-context
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
    /// **The descriptor this session's credential presented for each satisfied term** — the one
    /// route by which a term ordinal becomes a string a viewer is shown (decision 0114).
    ///
    /// The drill-down's `labels` array is built from this map alone: an entity's own term list is
    /// intersected with it, and a term the map does not hold has no name here and cannot be
    /// served. That is the satisfied-only rule expressed as a data structure rather than as a
    /// filter — a bug in the intersection can lose a label the viewer holds, and cannot invent one
    /// they do not.
    ///
    /// **This is also why the bundle carries no reverse dictionary.** Resolving an ordinal to its
    /// descriptor globally would need an index over every term the corpus knows — at the plugin's
    /// declared 2×10⁸ terms, gigabytes of it — for a surface that may only ever name terms the
    /// caller already handed in. The credential's own descriptors are bounded by
    /// `max_terms_per_token` and are already in hand at authorise.
    ///
    /// `public` is here with a descriptor no credential supplied, exactly as it is in
    /// [`Session::satisfied`] and for the same reason: it is the label every principal holds.
    pub(crate) satisfied_descriptors: Arc<FxHashMap<TermId, Vec<u8>>>,
    /// **The visible-view set** (`views.md` §6): every view of every group this principal may
    /// reach, resolved once here at authorise and **fixed for this session's life**.
    ///
    /// Fixed is a guarantee rather than an oversight. Every view is evaluated at authorise
    /// whatever the outcome, so the request-time check is one set-membership lookup and a
    /// gate-failed name costs the same work as a name nobody declared — r23's
    /// work-indistinguishability standard, and the closure Appendix C's C4 records for
    /// `/v1/items`. A view **created after** this session authorised is therefore a 404 to it
    /// until it re-authorises (owner ruling 2026-08-30): creation is rare, tokens expire, and the
    /// alternatives — a per-request gate evaluation, or a lazily-evaluated miss — each cost
    /// exactly the property this field exists to hold. Roster immutability (`views.md` §3.2) is
    /// the other half: a gate, once written, never changes, so a fixed set can never hold a stale
    /// *widening*.
    ///
    /// `Arc` because every request path reads it and none of them may clone the set.
    pub visible_views: Arc<crate::gate::VisibleViews>,
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

/// One (partition, view)'s live segment count — [`Engine::live_segment_counts`]'s element, and
/// what `/control/status` publishes under `segments`.
///
/// **Plain `String`s and a `usize`, defined here rather than re-exported from `tessera-store`.**
/// `check-layers.sh` denies a `tessera-server → tessera-store` edge (SA §3), so a gauge the server
/// publishes must be nameable from this crate — the discipline `FragmentCacheStats` and
/// `DeclaredScalar` already establish at the crate root. This one owns nothing of the store's
/// vocabulary, so it is a definition here rather than a re-export.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ViewSegments {
    pub partition: String,
    pub view: String,
    /// Segments this view's viewport sweep would iterate — base plus every flush extent merge has
    /// not yet collapsed.
    pub segments: usize,
}

/// One partition's live geometry position — [`Engine::partition_status`]'s element, and what
/// `/control/status` publishes as contracts §3.4's per-partition block.
///
/// Defined here rather than re-exported from `tessera-store`, for [`ViewSegments`]' reason: the
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
/// The runtime attribute columns the side manifests carry (`ingest.md` §6.3), **in one order**.
///
/// A column's position in the served list is what every buffered row, record-blob tag and
/// segment tail is positional against, so the order the manifests' lists are appended in decides
/// which column a value is read under. The partitions are a hash map; they are walked by key,
/// ascending, so two opens of one bundle build the same list. With one partition this is that
/// partition's lists; with several, every declaration is a deployment-level fact every partition
/// publishes alike, and a name met again is skipped by `Manifest::with_attributes`.
/// The view groups and plain views the side manifests carry (`ingest.md` §1.3), on
/// [`side_manifest_vocabularies`]' rule: the partitions are walked by key, ascending, and a name
/// met again is skipped by the manifest merge.
fn side_manifest_view_declarations(
    bundle: &Bundle,
) -> (
    Vec<tessera_store::manifest::GroupDescriptor>,
    Vec<tessera_store::manifest::ViewDescriptor>,
) {
    let mut keys: Vec<&String> = bundle.partitions.keys().collect();
    keys.sort();
    let mut groups: Vec<tessera_store::manifest::GroupDescriptor> = Vec::new();
    let mut plain: Vec<tessera_store::manifest::ViewDescriptor> = Vec::new();
    for key in keys {
        let manifest = &bundle.partitions[key].manifest;
        for group in &manifest.groups {
            if !groups.iter().any(|held| held.name == group.name) {
                groups.push(group.clone());
            }
        }
        for view in &manifest.plain_views {
            if !plain.iter().any(|held| held.id == view.id) {
                plain.push(view.clone());
            }
        }
    }
    (groups, plain)
}

fn side_manifest_vocabularies(bundle: &Bundle) -> Vec<tessera_store::manifest::ManifestVocabulary> {
    let mut keys: Vec<&String> = bundle.partitions.keys().collect();
    keys.sort();
    let mut out: Vec<tessera_store::manifest::ManifestVocabulary> = Vec::new();
    for key in keys {
        for vocabulary in &bundle.partitions[key].manifest.vocabularies {
            if !out.iter().any(|held| held.name == vocabulary.name) {
                out.push(vocabulary.clone());
            }
        }
    }
    out
}

fn side_manifest_attributes(
    bundle: &Bundle,
) -> (
    Vec<tessera_store::manifest::DeclaredScalar>,
    Vec<tessera_store::manifest::ScopedScalar>,
) {
    let mut keys: Vec<&String> = bundle.partitions.keys().collect();
    keys.sort();
    let mut attributes = Vec::new();
    let mut scoped = Vec::new();
    for key in keys {
        let manifest = &bundle.partitions[key].manifest;
        for d in &manifest.attributes {
            if !attributes
                .iter()
                .any(|held: &tessera_store::manifest::DeclaredScalar| held.name == d.name)
            {
                attributes.push(d.clone());
            }
        }
        for f in &manifest.scoped_attributes {
            if !scoped
                .iter()
                .any(|held: &tessera_store::manifest::ScopedScalar| held.name == f.name)
            {
                scoped.push(f.clone());
            }
        }
    }
    (attributes, scoped)
}

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
        let side_manifest_high_waters: Vec<u64> = bundle
            .partitions
            .values()
            .map(|partition| partition.manifest.entity_id_high_water)
            .collect();
        // **The row-less mark's homes are the side manifests only**, and `SEGMENTS-0.json` is one
        // of them — a build whose declaration carries layers spends row-less ids and records the mark there, so
        // this is where a built layer's claim is honoured. `MANIFEST.json` carries no such field at
        // all, and folding the ceiling in as the bundle term is what says "nothing row-less yet"
        // without inventing one.
        let side_manifest_low_waters: Vec<u64> = bundle
            .partitions
            .values()
            .map(|partition| partition.manifest.entity_id_low_water)
            .collect();
        // One partition today, so this concatenation is the whole registry; at more than one it is
        // the union, and a layer registered against one partition is a layer of the deployment
        // (⊘ **I13b's obligation lands here at the first second partition** — the registry itself
        // is partition-independent, but nothing yet checks that two partitions agree about a name).
        let manifest_layers: Vec<tessera_types::layer::RegisteredLayer> = bundle
            .partitions
            .values()
            .flat_map(|partition| partition.manifest.layers.iter().cloned())
            .collect();
        let manifest_layer_tombstones: Vec<String> = bundle
            .partitions
            .values()
            .flat_map(|partition| partition.manifest.layer_tombstones.iter().cloned())
            .collect();
        // **The roster's runtime half, unioned on `manifest_layers`' argument** (`views.md` §3.2):
        // a view is a deployment-level object — its key is the group's, not a partition's — so
        // the creations and the tombstones belong to the deployment whichever
        // partition's manifest published them. With one partition this is that partition's list.
        let manifest_created_views: Vec<tessera_types::view::CreatedView> = bundle
            .partitions
            .values()
            .flat_map(|partition| partition.manifest.views.iter().cloned())
            .collect();
        let manifest_dead_incarnations: Vec<tessera_types::view::DeadIncarnation> = bundle
            .partitions
            .values()
            .flat_map(|partition| partition.manifest.dead_view_incarnations.iter().cloned())
            .collect();
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
        let manifest_membership_extents: Vec<tessera_store::manifest::MembershipExtent> = bundle
            .partitions
            .values()
            .flat_map(|partition| partition.manifest.membership_extents.iter().cloned())
            .collect();
        // The two lists that make a level's derived structures placeable across a restart, unioned
        // on the same argument.
        let manifest_level_versions: Vec<tessera_store::manifest::LevelVersion> = bundle
            .partitions
            .values()
            .flat_map(|partition| partition.manifest.level_versions.iter().cloned())
            .collect();
        let manifest_containment_extents: Vec<tessera_store::manifest::ContainmentExtent> = bundle
            .partitions
            .values()
            .flat_map(|partition| partition.manifest.containment_extents.iter().cloned())
            .collect();
        let manifest_tile_index_extents: Vec<tessera_store::manifest::TileIndexExtent> = bundle
            .partitions
            .values()
            .flat_map(|partition| partition.manifest.tile_index_extents.iter().cloned())
            .collect();
        let manifest_row_column_extents: Vec<tessera_store::manifest::RowColumnExtent> = bundle
            .partitions
            .values()
            .flat_map(|partition| partition.manifest.row_column_extents.iter().cloned())
            .collect();
        let manifest_shape_rows_extents: Vec<tessera_store::manifest::ShapeRowsExtent> = bundle
            .partitions
            .values()
            .flat_map(|partition| partition.manifest.shape_rows_extents.iter().cloned())
            .collect();
        let manifest_shape_held_extents: Vec<tessera_store::manifest::ShapeHeldExtent> = bundle
            .partitions
            .values()
            .flat_map(|partition| partition.manifest.shape_held_extents.iter().cloned())
            .collect();
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
        // The mask this engine opens with, derived from the overlay replay reconstructed and the
        // row space the bundle carries — the same derivation every later publication repeats
        // (`compose::derive_denied`). A node restarting into a live suppression set gets it here,
        // not on its first request.
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
        let scoped_columns: Vec<(String, String, tessera_types::view::ViewIncarnation)> = bundle
            .partitions
            .values()
            .flat_map(|p| p.manifest.scoped_columns.iter())
            .map(|c| (c.column.clone(), c.view.clone(), c.incarnation))
            .collect();
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
            let artifact_record_extents = bundle
                .partitions
                .get(&partition)
                .map(|p| p.manifest.artifact_record_extents.clone())
                .unwrap_or_default();
            let text_extents = bundle
                .partitions
                .get(&partition)
                .map(|p| p.manifest.text_extents.clone())
                .unwrap_or_default();
            // The entity→term transpose rides the same open (contracts §2.4): the base the build
            // always writes plus every extent the side-manifest names, so a restart composes the
            // labels of everything flushed since the build rather than answering "unknown" for it.
            let entity_terms_extents = bundle
                .partitions
                .get(&partition)
                .map(|p| p.manifest.entity_terms_extents.clone())
                .unwrap_or_default();
            Arc::new(
                crate::filter::FilterColumns::open(
                    &prefix_dir,
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
                    &extents,
                    &record_extents,
                    &artifact_record_extents,
                    &entity_terms_extents,
                    &text_extents,
                    // The columns whose base no fold has written yet (`ingest.md` §6.3).
                    &unfolded_attributes,
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

        let generation = Arc::new(ArcSwap::new(Arc::new(Generation {
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
            denied,
        })));

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
        artifact_projections.adopt_all(
            &prefix_dir,
            generation.load().prefix.as_str(),
            &manifest_containment_extents,
            &write_state.artifacts,
        );
        // **And the tile indexes beside them, at the same point and under the same rule** — the
        // level versions have to be the seeded ones plus the replay, and the direction of a
        // mistaken adoption is the mirror image of the partition's: a stale index is *narrow*, and
        // a narrow extent settles an artifact whose membership is not inside the viewport.
        artifact_projections.adopt_indexes(
            &prefix_dir,
            generation.load().prefix.as_str(),
            &manifest_tile_index_extents,
            &write_state.artifacts,
        );
        // **And the row-major columns, at the same point and under the same rule.** A stale column
        // is narrow in the same way an index is: a growth added rows it does not label, and an
        // unlabelled row is one no artifact claims — so the artifact holding it stops being a
        // candidate there and its masked count comes back short.
        artifact_projections.adopt_columns(
            &prefix_dir,
            generation.load().prefix.as_str(),
            &manifest_row_column_extents,
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
            containment_named = manifest_containment_extents.len(),
            containment_adopted = artifact_projections.adopted(),
            tile_indexes_named = manifest_tile_index_extents.len(),
            row_columns_named = manifest_row_column_extents.len(),
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
                    shape_rows: &manifest_shape_rows_extents,
                    row_columns: &manifest_row_column_extents,
                    shape_held: &manifest_shape_held_extents,
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

    /// Turn the background occupancy fill off, so a request computes every rung itself.
    ///
    /// **A test hook, gated so it cannot exist in a shipped build**, on
    /// [`Self::set_background_refresh_for_test`]'s argument exactly. A test asserting what a
    /// *request* computed — that the anchor is the composed figure, that a suppression moves it —
    /// would otherwise be answered from a rung a background task filled, and would go on passing
    /// with the request path removed. It must be set **before** the viewport that would spawn the
    /// fill, which is any request that takes θ's anchor below `stage::BACKGROUND_DEPTH`.
    #[cfg(feature = "fault-injection")]
    #[doc(hidden)]
    pub fn set_occupancy_stage_for_test(&self, enabled: bool) {
        self.stage.enabled.store(enabled, Ordering::SeqCst);
    }

    /// How many background occupancy fills are still running — see [`crate::stage`].
    ///
    /// **A test hook**, on [`Self::set_background_refresh_for_test`]'s argument: a test that wants
    /// the filled rungs in place polls this rather than sleeping on a guess about the pool.
    #[cfg(feature = "fault-injection")]
    #[doc(hidden)]
    pub fn occupancy_stages_in_flight(&self) -> usize {
        self.stage.in_flight()
    }

    /// Drop one vocabulary's suggestion index from the live generation — the fault state
    /// [`EngineError::SuggestionUnavailable`] exists for.
    ///
    /// **A test hook, gated so it cannot exist in a shipped build**, on
    /// [`Self::set_background_refresh_for_test`]'s argument. Nothing request-shaped reaches that
    /// refusal: `Engine::open` builds an index for every vocabulary a declared category column
    /// names, and only a build that failed at open leaves one absent — which is a host condition a
    /// test cannot produce without either breaking the filesystem or reaching in here.
    ///
    /// **It submits to the executor rather than swapping the generation itself**, and that is not
    /// ceremony. The executor thread is the sole publisher (lifecycle §1.3, #59): it loads the live
    /// generation, builds a successor and stores it, so a store from any other thread can be
    /// overwritten by a swap already in flight between those two steps. A hook that lost its swap
    /// that way would leave the test asserting against an index it had asked to remove — passing or
    /// failing on timing rather than on the behaviour under test — and `scripts/check-layers.sh`
    /// refuses the second publisher for exactly that reason. Returns once the executor has
    /// published, so the caller's next request sees it.
    ///
    /// Requires a started write executor; `false` where there is none.
    #[cfg(feature = "fault-injection")]
    #[doc(hidden)]
    pub fn forget_suggestion_index_for_test(&self, vocabulary: &str) -> bool {
        self.write.forget_suggestion_index(vocabulary.to_string())
    }

    /// Rebuild one vocabulary's suggestion index from the live minter and publish it, returning
    /// once the executor has swapped — the cadence a fixture cannot otherwise reach.
    ///
    /// **A test hook, gated so it cannot exist in a shipped build**, on
    /// [`Self::forget_suggestion_index_for_test`]'s argument, and submitted through the executor
    /// for that method's reason. It exists because a rebuild is dispatched only when a side map has
    /// run 4,096 values ahead of its base — hundreds of ingest batches — and because it is the one
    /// publication that moves neither `segments_version` nor `overlay_version`, which makes it
    /// exactly the state a per-session suggestion set's key cannot see
    /// (`crate::suggest_set::SuggestSets::get`).
    ///
    /// Requires a started write executor; `false` where there is none.
    #[cfg(feature = "fault-injection")]
    #[doc(hidden)]
    pub fn rebuild_suggestion_index_for_test(&self, vocabulary: &str) -> bool {
        self.write.rebuild_suggestion_index(vocabulary.to_string())
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

    /// Hold a **completed** merge in its channel, undrained, so a flush can publish **inside the
    /// merge's flight** — see [`crate::write::MaintenanceDeps::merge_publication_paused`] for the
    /// window and why it is a real one. [`Self::set_fold_publication_paused_for_test`]'s shape,
    /// including the wake: the executor draining nothing is what parks it, so unpausing must ring
    /// the doorbell.
    pub fn set_merge_publication_paused_for_test(&self, paused: bool) {
        self.merge_publication_paused
            .store(paused, Ordering::SeqCst);
        if !paused {
            self.write.wake();
        }
    }

    /// Whether a completed fold is waiting, undrained, at
    /// [`Self::set_fold_publication_paused_for_test`]'s hold — the fold's half of
    /// [`Self::merge_publication_is_held_for_test`], and the condition a test waits on rather than
    /// guessing at the hold with a sleep.
    pub fn fold_publication_is_held_for_test(&self) -> bool {
        self.write
            .health()
            .fold_completed_pending
            .load(Ordering::SeqCst)
    }

    /// Whether a completed merge is waiting, undrained, at
    /// [`Self::set_merge_publication_paused_for_test`]'s hold. The condition a test waits on
    /// instead of guessing at the hold with a sleep.
    pub fn merge_publication_is_held_for_test(&self) -> bool {
        self.write
            .health()
            .merge_completed_pending
            .load(Ordering::SeqCst)
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

    /// **Whether this layer's levels may be served from their column alone** — the registry's
    /// answer to [`crate::artifacts::serves_column_only`], asked here so every call site agrees.
    /// A layer this engine does not carry keeps the artifact-major form, which is the conservative
    /// reading of a name the registry cannot resolve.
    pub(crate) fn serves_column_only(&self, layer: &str) -> bool {
        self.write
            .registered_layer(layer)
            .is_some_and(|registered| crate::artifacts::serves_column_only(&registered.declaration))
    }

    /// `member_of` leaves served by the row-column walk rather than by the artifact-major
    /// membership — see [`Self::member_of_column_walks`].
    pub fn member_of_column_walks(&self) -> u64 {
        self.member_of_column_walks.load(Ordering::Relaxed)
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
        // The descriptor beside each ordinal, kept for the drill-down's `labels` array — see
        // `Session::satisfied_descriptors`. Populated from the credential's own bytes and from
        // nothing else, which is what makes the surface satisfied-only by construction.
        let mut satisfied_descriptors: FxHashMap<TermId, Vec<u8>> = FxHashMap::default();
        let mut unresolved_count = 0usize;
        for descriptor in &auth_terms.terms {
            match generation.dict.lookup(descriptor) {
                Some(term) => {
                    satisfied.insert(term);
                    satisfied_descriptors.insert(term, descriptor.clone());
                }
                // An unknown descriptor is simply unsatisfied, never an error — and §3.3's
                // observation is that the ones that drop out here are precisely this session's
                // exposure to a later promotion, so the condition costs a counter to keep and a
                // rebuild to recover.
                None => unresolved_count += 1,
            }
        }

        // **`public` is added here, inside the trust boundary, and nowhere else.**
        // It is the one label every principal holds (`per-point-attributes.md` §3.8), and where it
        // is added decides what it is worth. Not as a grant, which would make the corpus's only
        // universal label depend on every credential being issued correctly; not in the plugin,
        // which is caller-supplied code deciding what a credential's bytes mean; here, after the
        // credential has been resolved and before anything is masked with the result.
        //
        // **Resolved by descriptor, not asserted as term `0`.** Every build interns it first, so
        // the two are the same number in every bundle this build writes — but a bundle whose
        // dictionary does not carry the label at all would, under a hardcoded `0`, hand every
        // principal whichever descriptor happened to be interned first. Looking the label up costs
        // one dictionary probe per authorise and cannot fail open: a bundle without it adds
        // nothing, which is the narrow direction.
        if let Some(term) = generation.dict.lookup(tessera_authz::PUBLIC_LABEL) {
            debug_assert_eq!(
                term,
                tessera_authz::PUBLIC_TERM,
                "`public` is reserved at term 0 by every build"
            );
            satisfied.insert(term);
            satisfied_descriptors.insert(term, tessera_authz::PUBLIC_LABEL.to_vec());
        }

        // **The visible-view set, resolved here and never again** (`views.md` §6) — after the
        // credential has been resolved and `public` added, and before anything is masked with the
        // result, because the gate is satisfied by exactly the terms an item's label is. Every
        // view of every group is evaluated whatever the outcome; see `crate::gate`.
        let visible_views = Arc::new(crate::gate::resolve(
            &generation.bundle.manifest,
            &generation.dict,
            &satisfied,
            self.plugin.as_ref(),
        ));

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
            satisfied_descriptors: Arc::new(satisfied_descriptors),
            visible_views,
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
        // **First, and before anything is dropped.** A ladder fill still running for this token
        // would otherwise re-publish the very occupancy entries this call is removing — the
        // residue would be bounded and benign, but it would also be work done on behalf of a
        // session that no longer exists.
        self.stage.cancel(token_id);
        // **Both per-session caches**, and the second one is not optional hygiene at the campaign's
        // target: a masked-count histogram is ~4 B per artifact, 40 MB at 10⁷, and a revoked
        // session's is pinned by nothing else.
        self.masked_counts.prune_token(token_id);
        self.occupancy.retain_keys(|key| key.token_id != token_id);
        self.derived_geometry.prune_token(token_id);
        self.suggest_sets.prune_token(token_id);
        self.row_projection_cache.prune_token(token_id)
    }

    /// Drop every entry belonging to any of `token_ids` — the expiry sweep's form of
    /// [`Self::prune_token`], and the same removal with the same argument.
    ///
    /// **One pass per cache, not one prune per session.** A sweep drops a batch, and five
    /// `retain_keys` passes per session would take each of the five mutexes once per victim and
    /// re-walk every surviving key each time. Here each cache is walked once with a set membership
    /// test, so the cost is O(entries) in the batch rather than O(entries × victims). The stage
    /// cancellations stay per token: each is a map removal under its own lock, and there is no
    /// walk to share.
    ///
    /// Returns how many row projections were removed, as [`Self::prune_token`] does. Call it off
    /// the request path — `tessera_server`'s `/session/authorise` hands it to `spawn_blocking`,
    /// beside the authorisation it already runs there.
    pub fn prune_tokens(&self, token_ids: &FxHashSet<u64>) -> usize {
        if token_ids.is_empty() {
            return 0;
        }
        for token_id in token_ids {
            self.stage.cancel(*token_id);
        }
        self.masked_counts.prune_tokens(token_ids);
        self.occupancy
            .retain_keys(|key| !token_ids.contains(&key.token_id));
        self.derived_geometry.prune_tokens(token_ids);
        self.suggest_sets.prune_tokens(token_ids);
        self.row_projection_cache.prune_tokens(token_ids)
    }

    /// The masked-count cache's gauges — see [`crate::histogram::MaskedCountStats`]. Operator plane
    /// only; a count of structures, naming no artifact and no principal.
    pub fn masked_count_cache_stats(&self) -> crate::histogram::MaskedCountStats {
        self.masked_counts.stats()
    }

    /// The derived-geometry cache's gauges — see [`crate::derived_cache::DerivedCacheStats`].
    /// Operator plane only; a count of structures, naming no artifact and no principal.
    ///
    /// `hit_rate` is the figure this cache is judged on, and it is a figure about a *pan*: the same
    /// principal panning across one layer re-serves mostly the same artifacts, which is what makes
    /// a held shape worth its bytes.
    pub fn derived_cache_stats(&self) -> crate::derived_cache::DerivedCacheStats {
        self.derived_geometry.stats()
    }

    /// Bound the derived-geometry cache. An embedder that never calls this gets
    /// `crate::derived_cache`'s own default, which is where the figure is argued — there is no
    /// configuration key, because an entry's size is bounded by the vertex budget rather than by
    /// the corpus.
    pub fn set_derived_cache_bytes(&self, bytes: u64) {
        self.derived_geometry.set_bound_bytes(bytes);
    }

    /// How many levels are recorded row-major and served artifact-major — see
    /// [`crate::artifacts::ArtifactProjections::layout_fallbacks`].
    pub fn layout_fallbacks(&self) -> u64 {
        self.artifact_projections.layout_fallbacks()
    }

    /// How many fold-written row-major columns were claimed rather than composed.
    pub fn columns_adopted(&self) -> u64 {
        self.artifact_projections.columns_adopted()
    }

    /// How many row-major columns were composed from a level's row form rather than claimed — see
    /// [`crate::artifacts::ArtifactProjections::columns_composed`].
    pub fn columns_composed(&self) -> u64 {
        self.artifact_projections.columns_composed()
    }

    /// The serving layout recorded for one `(layer, level)`, or `None` where no such layer is
    /// registered. Operator plane only: it names no artifact and no principal, and nothing on the
    /// wire carries it.
    pub fn recorded_layout(
        &self,
        layer: &str,
        level: u32,
    ) -> Option<tessera_types::layer::ServingLayout> {
        self.write
            .registered_layer(layer)
            .map(|registered| registered.layout_of(level))
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

    /// Bound the masked-count cache (`serve.masked_count_cache_bytes`).
    ///
    /// **Its own setter rather than a third argument to [`Self::set_cache_bounds`]**, because the
    /// two callers are different: every embedder calls that one through `tessera_server::prepare`,
    /// and this key exists for a deployment that has a row-major layer at all — which is a property
    /// of the corpus rather than of the box. An embedder that never calls it gets an unbounded
    /// cache, which is what a read-only embedder over a small corpus wants.
    pub fn set_masked_count_cache_bytes(&self, bytes: u64) {
        self.masked_counts.set_bound_bytes(bytes);
    }

    /// Bound the region decomposition cache (`serve.region_cache_bytes`) — a setter for
    /// [`Self::set_masked_count_cache_bytes`]'s reason.
    pub fn set_region_cache_bytes(&self, bytes: u64) {
        self.region_cache.set_bound_bytes(bytes);
    }

    /// `serve.max_region_cells` — the boundary-cell budget a region's descent stops at
    /// (selection-operand §6; published on `/v1/meta`). A setter rather than an `EngineConfig`
    /// field, for [`Self::set_masked_count_cache_bytes`]'s reason; the default is
    /// [`crate::region::DEFAULT_MAX_REGION_CELLS`].
    pub fn set_max_region_cells(&self, cells: usize) {
        self.max_region_cells.store(cells as u64, Ordering::Relaxed);
    }

    /// The region cache's gauges, beside the row-projection cache's.
    pub fn region_cache_stats(&self) -> crate::single_flight::CacheStats {
        self.region_cache.stats()
    }

    /// The occupancy memo's gauges — one entry per `(session, view, depth, generation)` rung of
    /// θ's `N_occ` ladder. Operator plane only; a count of structures, naming no principal.
    ///
    /// `evictions` rising is the memo doing what its bound is for: the entries it removes are
    /// rungs taken against a superseded generation, which no request can ask for again.
    pub fn occupancy_cache_stats(&self) -> crate::single_flight::CacheStats {
        self.occupancy.stats()
    }

    /// Bound the occupancy memo. An embedder that never calls this gets
    /// [`crate::occupancy::DEFAULT_OCCUPANCY_CACHE_BYTES`], which is where the figure is argued.
    /// A setter rather than an `EngineConfig` field, for [`Self::set_masked_count_cache_bytes`]'s
    /// reason.
    pub fn set_occupancy_cache_bytes(&self, bytes: u64) {
        self.occupancy.set_bound_bytes(bytes);
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

    /// Live segments per (partition, view), read straight off the current generation — the gauge
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
    /// Sorted by `(partition, view)` because the generation holds them in `HashMap`s: an operator
    /// diffing two status responses must not see a reordering that means nothing.
    pub fn live_segment_counts(&self) -> Vec<ViewSegments> {
        let generation = self.generation.load();
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

    /// Projection builds split by route, in [`crate::compose::ProjectionRoute::ALL`]'s order —
    /// the request path's and the background refresh's together, so it does not sum to
    /// [`Self::full_projection_builds`].
    ///
    /// The distribution is what says whether a deployment's term images are earning anything: a
    /// corpus whose images are written and never read is one whose keep rule or whose chooser
    /// constants do not match the principals it actually serves.
    pub fn projection_builds_by_route(&self) -> [u64; 4] {
        self.projection_routes.counts()
    }

    /// Take every projection build by `route` rather than by the one the chooser prices, or by the
    /// chosen route again on `None`.
    ///
    /// **A test hook, gated so it cannot exist in a shipped build**, on
    /// [`Self::set_background_refresh_for_test`]'s argument. What it is for is the property that
    /// the routes agree: a test that only compared chosen routes would compare one route with
    /// itself, because the chooser picks the same one for the same principal every time.
    ///
    /// A forced route with nothing to run — a split where the view has no images or the session
    /// holds no term with one, a complement over a base that does not record the row count its
    /// slots are a bijection onto, a whole-domain answer over a grant that is not whole — walks
    /// instead, and
    /// [`Self::projection_builds_by_route`] records the walk. A caller asserting that its forced
    /// route ran reads the gauge.
    #[cfg(feature = "fault-injection")]
    #[doc(hidden)]
    pub fn force_projection_route_for_test(&self, route: Option<crate::compose::ProjectionRoute>) {
        self.projection_routes.force(route);
    }

    /// The rows this session's row projection holds in `view`, built or served exactly as a
    /// viewport would reach it.
    ///
    /// **A test hook, gated so it cannot exist in a shipped build.** A row projection is a
    /// per-session cache entry behind the single-flight ladder and reaches no public surface: the
    /// answers it produces do, and a test that only compared answers could not tell a projection
    /// that lost rows from a request that was never going to return them. The suite that checks
    /// the three routes against each other compares the projections themselves, which is the
    /// claim.
    #[cfg(feature = "fault-injection")]
    #[doc(hidden)]
    pub fn session_projection_rows_for_test(
        &self,
        session: &Session,
        view: &str,
    ) -> Result<croaring::Bitmap> {
        let generation = self.generation.load_full();
        let view_data = generation
            .bundle
            .partitions
            .values()
            .find_map(|partition| partition.views.get(view))
            .ok_or_else(|| EngineError::UnknownView(view.to_string()))?;
        let mut probe = crate::timing::Probe::new();
        let geometry =
            self.session_geometry(session, &generation, view, view_data, &None, &mut probe)?;
        Ok(geometry.projection.bitmap().clone())
    }

    /// The same rows by the walk, built here and cached nowhere — the reference every route is
    /// compared against.
    ///
    /// **A test hook, gated so it cannot exist in a shipped build**, on
    /// [`Self::session_projection_rows_for_test`]'s argument. It goes through `RowSpace::project`
    /// rather than through [`crate::compose::ProjectionRoute::Walk`] so that the reference is the
    /// row space's own crossing and not the same code path under another name.
    #[cfg(feature = "fault-injection")]
    #[doc(hidden)]
    pub fn session_walk_rows_for_test(
        &self,
        session: &Session,
        view: &str,
    ) -> Result<croaring::Bitmap> {
        let generation = self.generation.load_full();
        let view_data = generation
            .bundle
            .partitions
            .values()
            .find_map(|partition| partition.views.get(view))
            .ok_or_else(|| EngineError::UnknownView(view.to_string()))?;
        let fragment = self.fragment_for(session, &generation)?;
        let mut rows = view_data.row_space.project(&fragment.view());
        rows.run_optimize();
        Ok(rows)
    }

    /// How many walks of the mask and the Morton column this engine has made to resolve a rung of
    /// θ's occupied-tile anchor — see [`crate::occupancy`] and [`crate::stage`].
    ///
    /// **The number to watch is walks per session per publication**, and it should be one per view.
    /// One walk fills every rung at or below the depth it ran at, and the background fill takes it
    /// to `stage::BACKGROUND_DEPTH`, so a session that pans and zooms inside that range should move
    /// this only when the geometry or the overlay does. A deployment where it climbs with request
    /// volume is one where the memo is missing — the key carries the fragment's watermark, so a
    /// flush per request would do it.
    pub fn occupancy_walks(&self) -> u64 {
        self.occupancy_walks.load(Ordering::Relaxed)
    }

    /// Whether the external-id sidecar has opened any extent (or its locator) yet — exposed for
    /// tests confirming `Engine::open` never touches it — the per-extent laziness guarantee.
    pub fn external_id_sidecar_is_open(&self) -> bool {
        self.generation.load().external_index.0.is_open()
    }

    /// Whether the served bundle holds any external-id run: the build's (`--mint-external-ids`)
    /// or one a flush wrote from caller-supplied ids.
    ///
    /// For a route that addresses items by external id and finds that none of the ids it was
    /// given resolve. Against a bundle with no run, that is not a list of ids that name nothing;
    /// it is a deployment nothing can be named in by an external id, and the refusal should say
    /// so once rather than once per member. The check is not inside [`Self::resolve_external_ids`]
    /// because that call also serves `/control/ingest`'s duplicate check, where a deployment with
    /// no external ids yet is the ordinary state of a first batch into an empty database.
    ///
    /// Reads the manifest's run list; opens nothing. Ids ingested with an external id and not yet
    /// flushed live in the write path's live map, which this does not consult: a caller resolves
    /// its ids first, and asks this only when none resolved.
    pub fn bundle_carries_external_ids(&self) -> bool {
        self.generation.load().external_index.0.has_runs()
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
    /// whole of the misdirection guard: a shard that is not this one, or an entity in a range the
    /// allocator has never issued from, cannot name an item this deployment ever issued. Both are
    /// facts about the identifier space rather than about any item's visibility, and this is the
    /// admin plane (R5), so refusing precisely discloses nothing a caller could not compute.
    ///
    /// **The issued range is two ranges, and reading it as one is how layer suppression breaks.**
    /// Points are below the high-water mark; row-less entities — a layer's own, so that a
    /// suppression against it is an ordinary `/control/changes` entry — are at or above the
    /// row-less mark. A single `entity < high_water` test refuses every layer identifier this
    /// deployment has ever handed out, and the symptom is not an error anyone would connect to
    /// this line: it is that suppressing a layer answers *no such thing*. What names nothing is the
    /// **gap between the marks**, which is exactly the unissued space.
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
        let low_water = self.allocator_low_water();
        Ok(ids
            .iter()
            .map(|id| {
                let (id_shard, entity) = self.identity_key.invert(*id);
                let issued = entity.raw() < high_water || entity.raw() >= low_water;
                (id_shard == shard && issued).then_some(entity)
            })
            .collect())
    }

    /// Does `view` hold a row for `entity` — **the "already in the view" arm of the ingest join
    /// rule** (`views.md` §4)?
    ///
    /// "In the view" is the view's permutation **and** the commit window's buffer: a row accepted
    /// but not yet flushed is in no permutation, and a check that missed it would let two batches
    /// hand one flush two rows for one entity in one view, which the single-valued permutation
    /// cannot hold. The window's own open entries are covered upstream, by the early close a held
    /// external id already forces.
    ///
    /// One permutation read and one hash lookup; nothing walks.
    pub fn view_holds(&self, entity: EntityId, view: &str) -> bool {
        let generation = self.generation();
        generation.bundle.partitions.values().any(|partition| {
            partition
                .views
                .get(view)
                .is_some_and(|data| data.row_space.row_of(entity).is_some())
        }) || generation.buffer.contains_in_view(entity, view)
    }

    /// An already-flushed entity's **full** term set, ascending, from the entity→term transpose
    /// (contracts §2.4) — the join rule's label arm, once the entity's own row has left the buffer
    /// (`views.md` §4).
    ///
    /// **The full set, and it never leaves the server.** This is the opposite surface from the
    /// drill-down's `labels`, which serves the intersection with the asking session: this compares
    /// a *writer's* batch against what the deployment already holds, and equality is the whole
    /// question — an arm that compared only the terms the writer named would accept a batch that
    /// dropped one. Nothing derived from it is returned; the refusal names the row, never a term.
    ///
    /// `None` where no layer holds a list for the entity, which is *unknown* rather than *empty*
    /// and leaves the comparison unavailable exactly as an empty buffer does. `Some(vec![])` is a
    /// real answer: an item may legitimately carry no label.
    ///
    /// **A malformed layer is `None`, not a wrong answer — and it is logged, not swallowed.** The
    /// transpose refuses a bad offset pair rather than truncating
    /// (`tessera_store::entity_terms`), and this is a *report*, not an authorisation: the join it
    /// guards is inert either way (a joining row carries no descriptors), so a corrupt artefact
    /// loses the refusal rather than turning a caller's batch into a server error. That is the
    /// recoverable-and-discloses-nothing side of the line, where the posture is *report loudly and
    /// let the operator decide* — so the warning below fires, and the same corruption is a hard
    /// error on the drill-down path, which propagates it.
    ///
    /// **The warning names the artefact and not the entity** (**I10**, contracts §4). The
    /// byte-scanner sweeps payloads *and logs* for entity ids, and `crate`'s store follows the
    /// external-ID sidecar's rule at the same standard: the error carries the file and the shape
    /// of the inconsistency, which is what an operator chasing a systematic build or flush defect
    /// needs, and naming the slot buys nothing an entity-independent message does not.
    pub fn flushed_terms(&self, entity: EntityId) -> Option<Vec<TermId>> {
        flushed_terms_of(&self.generation(), entity)
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

    /// What the open's shape warm pass did: how many segment pieces were claimed from the
    /// prefix's persisted forms and how many resolved from the geometry (`crate::shapes`).
    /// Operator plane only, beside [`Engine::write_executor_stats`]: counts of structures, naming
    /// no artifact and no principal.
    pub fn shape_warm_report(&self) -> crate::shapes::WarmReport {
        self.shapes.last_warm()
    }

    /// How many artifact row forms, and how many lineages, this engine has built since it opened.
    ///
    /// **The cadence, not the cost.** Both structures are per `(layer, level)` and both are
    /// rebuilt when that level's version moves; what these two numbers answer is how *often* that
    /// happens, which is the question `design/artifact-serving-at-scale.md` §8.1 and §8.2 are
    /// about and the one nothing reported while the store carried a single global version.
    /// Operator plane only, beside [`Engine::write_executor_stats`] — they count structures a
    /// deployment built, and name no artifact, no layer and no principal.
    pub fn artifact_cache_builds(&self) -> (u64, u64) {
        (self.artifact_projections.builds(), self.lineages.builds())
    }

    /// The row form this engine is **holding** for one `(view, layer, level)`, without building
    /// one — the maintained form itself, for the differential that asserts it equals a form built
    /// from scratch (`tests/artifact_bring_forward.rs`).
    ///
    /// **Test-only, and the reason is what it would otherwise be**: a caller that took the held
    /// form on a request path would be taking whatever was last written to the cache rather than
    /// the form of the generation it is serving — the freshness argument `get_or_build` makes by
    /// reading the level's version from the store it builds from.
    pub fn held_artifact_form_for_test(
        &self,
        view: &str,
        layer: &str,
        level: u32,
    ) -> Option<std::sync::Arc<crate::artifacts::ArtifactRows>> {
        self.artifact_projections.held_form(view, layer, level)
    }

    /// Drop every derived form this engine holds for one layer, so the next request builds them —
    /// the *from scratch* half of the same differential.
    pub fn forget_artifact_forms_for_test(&self, layer: &str) {
        self.artifact_projections.forget(layer);
    }

    /// How many artifact row forms, and how many lineages, are held right now.
    ///
    /// The gauge beside [`Engine::artifact_cache_builds`]'s counter, and the one that moves in
    /// both directions: a dropped layer's entries leave both caches at the drop. Operator plane
    /// only — counts of structures, naming no artifact, no layer and no principal.
    pub fn artifact_cache_held(&self) -> (usize, usize) {
        (self.artifact_projections.held(), self.lineages.held())
    }

    /// The supplied-content tables' gauges — see
    /// [`crate::artifact_content::ContentCacheStats`]. Its own accessor rather than a third
    /// element of the two tuples above, because it reports bytes as well as a count and those
    /// two report neither.
    ///
    /// Operator plane only, beside them: counts of structures a deployment built, naming no
    /// artifact, no layer and no principal.
    pub fn artifact_content_cache_stats(&self) -> crate::artifact_content::ContentCacheStats {
        self.level_contents.stats()
    }

    /// How many containment partitions this engine has composed (`crate::containment`).
    ///
    /// **Beside [`Engine::artifact_cache_builds`] because the interesting number is the ratio.**
    /// Under any plugin but the builtin this stays at zero while row forms keep being built, and
    /// containment is on the masked-count route everywhere: a deliberate, fail-closed state rather
    /// than a fault, and an operator has no other way to see it. It also stays below the row-form
    /// count where a bundle carries several views, because the expression is view-independent and
    /// is composed once for all of them. Operator plane only — it names no artifact, no layer and
    /// no principal.
    pub fn artifact_containment_partitions(&self) -> u64 {
        self.artifact_projections.partitions()
    }

    /// How many containment partitions this engine **adopted** from the prefix at open rather than
    /// composing (`crate::containment`).
    ///
    /// The other half of [`Engine::artifact_containment_partitions`]: a deployment that folded and
    /// then restarted should see this at the number of levels it holds and that one at zero. Both
    /// at zero with row forms being built is a foreign plugin; this at zero and that one climbing
    /// is every coordinate rejected — correct, and the expensive answer. Operator plane only.
    pub fn artifact_containment_partitions_adopted(&self) -> u64 {
        self.artifact_projections.adopted()
    }

    /// How many fold-written tile indexes this engine **claimed** from the prefix rather than
    /// deriving (`crate::tile_index`).
    ///
    /// Read beside [`Engine::artifact_cache_builds`]'s first number, which counts the row forms
    /// those indexes belong to: the two equal on a deployment that folded and restarted, and this
    /// one at zero says every coordinate was rejected or every level was published since the fold —
    /// correct, and the expensive answer. Operator plane only; it names no artifact, no layer and
    /// no principal.
    pub fn artifact_tile_indexes_adopted(&self) -> u64 {
        self.artifact_projections.indexes_adopted()
    }

    /// The last compaction fold's per-pass wall clock and resident set — empty before the first
    /// fold. Operator plane only, beside [`Engine::write_executor_stats`].
    pub fn last_fold_passes(&self) -> Vec<crate::compact::PassCost> {
        self.write.health().last_fold_passes()
    }

    /// What the last fold's deletions degraded: the artifacts that lost members, and the supplied
    /// content that lost a source (`annotation-write-cycle.md` §4.2).
    ///
    /// **Operator plane, and the counts here are unmasked** — this is the notice a *publisher* is
    /// owed about their own sets, outside the leak register's viewer scope
    /// ([decision 0024](../../../docs/decisions/0024-operator-credential-is-out-of-scope.md)). No
    /// viewer-facing route may carry these numbers.
    ///
    /// The durable copy is `reports/fold-<prefix>.json` in the bundle root, written before the
    /// fold retires anything and kept when the prefix it reports on is reclaimed. ⊘ No HTTP route
    /// serves this yet.
    pub fn last_fold_report(&self) -> Vec<tessera_lifecycle::membership::Degradation> {
        self.write.health().last_fold_report()
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

    /// Unflushed `POST /control/values` fills the buffer holds (`ingest.md` §1.4), entity-scoped
    /// and group-scoped together.
    ///
    /// **Test- and operator-facing, and what says whether a fill is still pinning the log.** A
    /// fill holds its `ValuesBatch` record against rotation until the flush that writes its cells
    /// consumes it, so a figure that never falls after a restart is the pin that would not
    /// release ([`IngestBuffer::oldest_wal_pos`]).
    pub fn buffered_fills(&self) -> usize {
        self.generation().buffer.fill_count()
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
    /// [`Engine::request_flush_publication`] is the same request with its publication number,
    /// which is the form the control plane takes; this one drops the number for a caller that
    /// only wants the tick pulled forward. One request path, so the flag is set under the
    /// publication lock whichever is called.
    pub fn request_flush(&self) {
        let _ = self.request_flush_publication();
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

    /// Submit an ingest batch whose rows name no artifacts — the plain form, and every batch that
    /// carries no membership column.
    ///
    /// One line of delegation rather than a second implementation: what a batch says about
    /// artifacts is a *field* of the command, and defaulting it here keeps the ordinary caller from
    /// having to spell an empty one.
    pub fn accept_ingest(
        &self,
        rows: Vec<UnallocatedRow>,
        batch_id: String,
        body_hash: [u8; 32],
    ) -> std::result::Result<Vec<EntityId>, crate::write::AcceptError> {
        self.accept_ingest_joining(rows, batch_id, body_hash, Default::default())
            .map(|(entity_ids, _)| entity_ids)
    }

    /// Submit an ingest batch and wait for its receipt. Rows arrive **unallocated**: entity ids are
    /// assigned on the executor, at the close of the commit window this submission lands in.
    ///
    /// `artifacts` is what a column named for a layer said — which artifacts these rows join, and
    /// which edges the adjacency of a list column declared (`artifacts-from-points.md` §6.2). It is
    /// resolved and grown **in the same commit as the rows**, so a batch is never half-applied: on
    /// a **closed** layer a key naming no artifact refuses the whole batch before an id is spent,
    /// on an **open** one it creates the artifact it names, and rows that were accepted carry their
    /// memberships from the moment they exist.
    ///
    /// Returns the assigned ids and **how many artifacts this batch created** — zero for every
    /// batch whose keys all existed, and the number a caller is owed because minting is not
    /// undoable (`artifacts-from-points.md` §3).
    ///
    /// Blocking — a tokio handler must call this inside `spawn_blocking`.
    pub fn accept_ingest_joining(
        &self,
        rows: Vec<UnallocatedRow>,
        batch_id: String,
        body_hash: [u8; 32],
        artifacts: tessera_lifecycle::BatchArtifacts,
    ) -> std::result::Result<(Vec<EntityId>, u64), crate::write::AcceptError> {
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
        // `declared_scalars`, so a row longer than the schema would pair values with columns that
        // do not exist. **A shorter row is lawful** (`ingest.md` §7.1): a column declared at a
        // running service appends at the tail, so a row decoded against the schema before the
        // declaration, or a batch that omits the column, holds nothing for it and is padded with
        // its absence at the window's close (`attributes::pad_to_schema`). Checked here for the
        // same reason the extent check is: the invariant is about the buffer, and the buffer has
        // more than one writer.
        let declared = self.meta().declared_scalars.len();
        if let Some((index, row)) = rows
            .iter()
            .enumerate()
            .find(|(_, row)| row.scalars.len() > declared)
        {
            return Err(crate::write::AcceptError::ScalarArity {
                index,
                expected: declared,
                got: row.scalars.len(),
            });
        }
        // **Each row against its own view's frame** (decision 0040). The extent is the view's,
        // so one bundle-wide check would pass a row that has no cell in the view it is destined
        // for — silently clamped onto that view's grid edge at the flush. A row naming a view
        // this bundle does not declare is refused here for the same reason: there is no frame to
        // check it against, and the handler's own 404 guards only one of the buffer's writers.
        let meta = self.meta();
        for (index, row) in rows.iter().enumerate() {
            let Some(quantisation) = meta.quantisation_of(&row.view) else {
                return Err(crate::write::AcceptError::UnknownView {
                    index,
                    view: row.view.clone(),
                });
            };
            if !quantisation.contains(row.x, row.y) {
                return Err(crate::write::AcceptError::OutsideExtent {
                    index,
                    x: row.x,
                    y: row.y,
                    quantisation,
                });
            }
        }
        self.write
            .accept_ingest(rows, batch_id, body_hash, artifacts)
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

    /// One registered layer's declaration, by name — **the control plane's lookup, with no gate**.
    ///
    /// It answers what a *declaration* says, never what is served: the viewer plane's question is
    /// [`Engine::visible_layers`], which resolves reachability per principal and asks the overlay
    /// live. This one exists for `/control/ingest`, which must decide whether a column names a
    /// layer, and for a caller already holding the operator credential that registered it.
    pub fn registered_layer(&self, name: &str) -> Option<tessera_types::layer::RegisteredLayer> {
        self.write.registered_layer(name)
    }

    /// Register an annotation layer, returning its `tessera_id`.
    ///
    /// That identifier is the only address by which the layer can later be suppressed — entity ids
    /// never cross the boundary (**I10**) — which is the whole reason a layer takes an entity.
    pub fn register_layer(
        &self,
        declaration: tessera_types::layer::LayerDeclaration,
    ) -> std::result::Result<TesseraId, crate::write::AcceptError> {
        let entity = self.write.register_layer(declaration)?;
        // The blinding is total over the space the allocator will issue — `try_with_marks` refuses
        // a seed at or above the ceiling, so a row-less entity is always inside it — which is why
        // this conversion cannot fail in practice and is unwrapped to a fail-closed refusal rather
        // than a new error shape.
        let generation = self.generation();
        self.identity_key
            .forward(generation.bundle.manifest.identity.shard_id, entity)
            .map_err(|_| {
                crate::write::AcceptError::Exec(tessera_lifecycle::ExecError::LayerRefused {
                    detail: "the layer's entity id lies outside the identity space".to_string(),
                })
            })
    }

    /// Drop a layer. Its name is tombstoned and refused on recreation for ever.
    pub fn drop_layer(&self, name: String) -> std::result::Result<(), crate::write::AcceptError> {
        self.write.drop_layer(name)
    }

    /// Declare an attribute column while the service runs (`PUT /control/attributes`;
    /// `ingest.md` §1.3, §6.3). Answers `true` where a column of that name already carried
    /// exactly this identity and nothing was appended.
    ///
    /// Blocking — a tokio handler must call this inside `spawn_blocking`.
    pub fn declare_attribute(
        &self,
        request: tessera_lifecycle::AttributeRequest,
    ) -> std::result::Result<bool, crate::write::AcceptError> {
        self.write.declare_attribute(request)
    }

    /// Fill attribute values on entities that already exist (`POST /control/values`;
    /// `ingest.md` §1.4). Nothing is allocated and no row is created: every cell that is absent
    /// takes the supplied value, one that already holds it is a no-op, and one that holds a
    /// different value refuses the whole batch.
    ///
    /// Blocking — a tokio handler must call this inside `spawn_blocking`.
    pub fn fill_values(
        &self,
        request: tessera_lifecycle::ValuesRequest,
    ) -> std::result::Result<crate::write::ValuesReceipt, crate::write::AcceptError> {
        self.write.fill_values(request)
    }

    /// Declare a vocabulary while the service runs (`PUT /control/vocabularies/{name}`;
    /// `ingest.md` §1.3). Answers `(existing, added, titles)`: whether a vocabulary of that name
    /// already carried this identity, how many of the request's values were novel, and how many
    /// held values it gave a new title.
    ///
    /// **Nothing is validated here**, on `register_layer`'s rule: whether the name is free, and
    /// what a held vocabulary's identity is, are state only the write executor may read.
    ///
    /// Blocking — a tokio handler must call this inside `spawn_blocking`.
    pub fn declare_vocabulary(
        &self,
        request: tessera_lifecycle::VocabularyRequest,
    ) -> std::result::Result<(bool, u64, u64), crate::write::AcceptError> {
        self.write.declare_vocabulary(request)
    }

    /// A page of values for a vocabulary that exists (`PATCH /control/vocabularies/{name}/values`;
    /// `ingest.md` §1.3). Answers `(added, existing, titles)`.
    ///
    /// Blocking — a tokio handler must call this inside `spawn_blocking`.
    pub fn mint_vocabulary_values(
        &self,
        vocabulary: String,
        values: Vec<tessera_lifecycle::DeclaredValue>,
    ) -> std::result::Result<(u64, u64, u64), crate::write::AcceptError> {
        self.write.mint_vocabulary_values(vocabulary, values)
    }

    /// Declare a view group while the service runs (`PUT /control/view_groups/{name}`;
    /// `ingest.md` §1.3). Answers `true` where a group of that name already carried exactly this
    /// identity and nothing was appended.
    ///
    /// **The gate's labels are checked here**, on [`Engine::create_view`]'s rule: whether the
    /// plugin can read a label is a question only the engine can ask. Every other rule is the
    /// write executor's.
    ///
    /// Blocking — a tokio handler must call this inside `spawn_blocking`.
    pub fn create_view_group(
        &self,
        declaration: tessera_lifecycle::wal::ViewGroupDeclaration,
    ) -> std::result::Result<bool, crate::write::AcceptError> {
        self.check_gate_labels(declaration.visibility.as_deref())?;
        self.check_point_default(declaration.point_default.as_deref())?;
        self.write.create_view_group(declaration)
    }

    /// Create a plain view while the service runs (`PUT /control/views/{name}`; `ingest.md` §1.3
    /// and §10, R9), on [`Engine::create_view_group`]'s rule.
    ///
    /// Blocking — a tokio handler must call this inside `spawn_blocking`.
    pub fn create_plain_view(
        &self,
        declaration: tessera_lifecycle::wal::PlainViewDeclaration,
    ) -> std::result::Result<bool, crate::write::AcceptError> {
        self.check_gate_labels(declaration.visibility.as_deref())?;
        self.check_point_default(declaration.point_default.as_deref())?;
        self.write.create_plain_view(declaration)
    }

    /// `point_visibility.default` — what a point carrying no label of its own is given
    /// (decision 0133) — measured against the plugin, on [`Engine::check_gate_labels`]' rule.
    ///
    /// **The consequence of getting this wrong lands on every unlabelled row, not on the
    /// declaration.** A default the plugin cannot turn into a term is one no principal holds, so
    /// every point that took it is in nobody's mask and the view fills with rows no viewer can
    /// see; refused here, the operator reads the message once. `inherited` and the empty string
    /// are the build's own two refusals (`tessera_build::config::check_label`), transcribed: the
    /// first is the reserved word for *the container's gate is the whole of it*, which for a
    /// point could only widen, and the second is no label at all.
    fn check_point_default(
        &self,
        default: Option<&str>,
    ) -> std::result::Result<(), crate::write::AcceptError> {
        let Some(default) = default else {
            return Ok(());
        };
        let refused = |detail: String| {
            crate::write::AcceptError::Exec(tessera_lifecycle::ExecError::ViewRefused { detail })
        };
        if default.trim().is_empty() {
            return Err(refused(
                "point_visibility.default is empty. An access label is a term a principal either \
                 holds or does not; write `public` for the one every principal holds"
                    .to_string(),
            ));
        }
        if default == "inherited" {
            return Err(refused(
                "point_visibility.default = \"inherited\" is refused. A container's gate narrows \
                 rather than widens, and a point carrying no terms is already in no principal's \
                 mask — so inheriting would have to *add* a term to the point, which can only \
                 widen it (configuration.md §4). Name the label such a point should carry, or \
                 `public`"
                    .to_string(),
            ));
        }
        let public = std::str::from_utf8(tessera_authz::PUBLIC_LABEL).expect("the label is ASCII");
        if default == public {
            return Ok(());
        }
        let descriptors = self
            .plugin
            .terms_of_labels(&[default.as_bytes().to_vec()])
            .map_err(|e| {
                refused(format!(
                    "point_visibility.default = {default:?} is not a label the plugin can read \
                     ({e}). It is given to every point that carries none of its own \
                     (decision 0133), so a label the plugin cannot turn into a term would put \
                     those points in no principal's mask"
                ))
            })?;
        if descriptors.is_empty() {
            return Err(refused(format!(
                "point_visibility.default = {default:?} names no terms, so every point given it \
                 would be in no principal's mask. Write `public`, or a label naming a term"
            )));
        }
        Ok(())
    }

    /// The half of a gate's validation that needs the plugin (`views.md` §6, decision 0132).
    ///
    /// A view's gate is satisfied by exactly the item-visibility predicate, so the labels go
    /// through the same `Plugin::terms_of_labels` call an item's `access` list takes at
    /// `/control/ingest`, each element one label taken verbatim. A list the plugin cannot read —
    /// an empty element, or no element at all — is refused rather than stored: stored, it would
    /// be a gate no principal could ever satisfy, including the operator who wrote it. `public`
    /// is not asked about — it is the label every principal holds inside the trust boundary
    /// (decision 0088) — and is recognised only as the whole of the list.
    fn check_gate_labels(
        &self,
        visibility: Option<&[String]>,
    ) -> std::result::Result<(), crate::write::AcceptError> {
        tessera_plugin::check_gate(self.plugin.as_ref(), visibility)
            .map(|_| ())
            .map_err(|detail| {
                crate::write::AcceptError::Exec(tessera_lifecycle::ExecError::ViewRefused { detail })
            })
    }

    /// Create a view of a view group while the service runs (`views.md` §3.2, decision 0108).
    ///
    /// **Almost nothing is validated here**, on `register_layer`'s rule: whether the key is free
    /// is state only the write executor may read — a handler that checked first could be
    /// overtaken between its check and the enqueue.
    ///
    /// The **gate's labels** are the exception, and they are here because only the engine holds
    /// the plugin: [`Engine::check_gate_labels`], the one site every gate on this plane is
    /// checked at.
    pub fn create_view(
        &self,
        group: String,
        key: String,
        visibility: Option<Vec<String>>,
        metadata: std::collections::BTreeMap<String, tessera_types::view::ViewMetadataValue>,
    ) -> std::result::Result<(), crate::write::AcceptError> {
        self.check_gate_labels(visibility.as_deref())?;
        self.write.create_view(group, key, visibility, metadata)
    }

    /// Drop a view. Its key is tombstoned and refused on recreation for ever, and the answer is
    /// how many entities `delete_dangling` submitted for deletion — zero unless it was asked for
    /// (`views.md` §3.4).
    pub fn drop_view(
        &self,
        group: String,
        key: String,
        delete_dangling: bool,
    ) -> std::result::Result<u64, crate::write::AcceptError> {
        self.write.drop_view(group, key, delete_dangling)
    }

    /// Put a batch of artifacts into one level of a layer, returning a `tessera_id` per artifact
    /// in the caller's submitted order and the batch's counts (`ingest.md` §1.5).
    ///
    /// A key the level holds is accepted under the fill rule and answers the held artifact's
    /// identifier; a key it does not hold is published (`LayerRegistry::prepare_put`).
    ///
    /// **Members must be points, and that is checked here.** An enumerated membership is a set of
    /// documents; a row-less entity — another layer, another artifact — has no row, so it would
    /// contribute to no masked count and to no tile, while still counting towards the declared
    /// size the proportional criterion divides by. An artifact could then be pushed below its own
    /// criterion by members that can never be visible to anyone. Relations between artifacts are a
    /// later stage's edges, not a membership.
    ///
    /// The check is against the point region's high-water mark, which only rises, so it cannot go
    /// stale between here and the executor.
    pub fn put_artifacts(
        &self,
        layer: String,
        level: u32,
        artifacts: Vec<tessera_lifecycle::IncomingArtifact>,
    ) -> std::result::Result<PublishedArtifacts, crate::write::AcceptError> {
        // Entity space is `u32` by I9, and both marks sit inside it — the row-less ceiling is
        // derived from `u32::MAX` — so the narrowing is total rather than merely usually safe.
        let high_water = self.allocator_high_water() as u32;
        let rowless: u64 = artifacts
            .iter()
            .map(|a| a.members.cardinality() - a.members.range_cardinality(0..high_water))
            .sum();
        if rowless > 0 {
            return Err(crate::write::AcceptError::Exec(
                tessera_lifecycle::ExecError::LayerRefused {
                    detail: format!(
                        "{rowless} member(s) of this batch name no point; a membership is a set of \
                         documents, and a member with no row would count towards the artifact's \
                         declared size while being visible to nobody"
                    ),
                },
            ));
        }

        // **A declared member that is deleted refuses the batch; a suppressed one is accepted**
        // (`annotation-write-cycle.md` §3.1). The two are not near-neighbours: a deleted entity can
        // never contribute to a count again and, inside a generating set, makes the content
        // unservable from birth — better a loud refusal than a description nobody can read and
        // nobody was told about. A suppressed entity is a live member temporarily outside every
        // mask, and both structures behave fail-closed until the unsuppress; refusing it would make
        // an operator's reversible action refuse a caller's unrelated publication.
        //
        // One `verdict` lookup per declared member, on the control plane, against the live overlay
        // — which cannot go stale in the wrong direction between here and the executor, a deletion
        // being irreversible.
        //
        // **The refusal reports a count and a position, never an entity id** (I10): the detail is
        // forwarded to the caller as the 422 body, and an entity id in it would cross the boundary.
        // **A view the layer's own group has no key for is refused** (`ingest.md` §1.5,
        // `views.md` §3.5), in the words the build refuses the same row in
        // (`tessera-build/src/layers.rs`): an artifact belongs to one view of the group its layer
        // is scoped to, and a key nobody declared is an artifact drawn nowhere — a publication
        // the operator asked for, acked, and then visible to no one. The group's roster is the
        // generation's, which is the build's views plus every view created while the service ran
        // (`views.md` §3.2), so a view created a moment ago passes.
        //
        // The refusal names the group's keys — operator-plane names on the control plane, which
        // decision 0024 puts outside the register's viewer scope.
        {
            let named: std::collections::BTreeSet<&str> = artifacts
                .iter()
                .filter_map(|artifact| artifact.view.as_deref())
                .collect();
            if !named.is_empty() {
                let scope = self
                    .registered_layer(&layer)
                    .and_then(|registered| registered.declaration.scope.group().map(String::from));
                if let Some(group) = scope {
                    let generation = self.generation();
                    let keys: Vec<&str> = generation
                        .bundle
                        .manifest
                        .groups
                        .iter()
                        .filter(|held| held.name == group)
                        .flat_map(|held| held.views.iter().map(|view| view.key.as_str()))
                        .collect();
                    if let Some(unknown) = named.iter().find(|view| !keys.contains(*view)) {
                        return Err(crate::write::AcceptError::Exec(
                            tessera_lifecycle::ExecError::LayerRefused {
                                detail: format!(
                                    "this batch names view '{unknown}', which group '{group}' \
                                     has no such key for. Its keys are: {}. An artifact belongs \
                                     to one view and its keys are unique per (layer, view), so a \
                                     key nobody declared is a refusal rather than an artifact \
                                     drawn nowhere (views §3.5)",
                                    keys.join(", ")
                                ),
                            },
                        ));
                    }
                }
            }
        }

        // **A membership spelled by exclusion carries no members here**: the complement is taken
        // on the executor, against the view's entity set with the deleted already out of it
        // (`ingest.md` §2.3), so both checks pass over the empty set the record carries at this
        // point and neither has anything to say about a list of entities to leave out.
        let generation = self.generation();
        let mut deleted = 0u64;
        let mut first_artifact = None;
        for (index, artifact) in artifacts.iter().enumerate() {
            let in_this = artifact
                .members
                .iter()
                .chain(
                    artifact
                        .contents
                        .iter()
                        .flat_map(|content| content.generated_from.iter()),
                )
                .filter(|entity| generation.overlay.is_deleted(EntityId::new(*entity as u64)))
                .count() as u64;
            if in_this > 0 {
                deleted += in_this;
                first_artifact.get_or_insert(index);
            }
        }
        if let Some(first_artifact) = first_artifact {
            return Err(crate::write::AcceptError::Exec(
                tessera_lifecycle::ExecError::LayerRefused {
                    detail: format!(
                        "{deleted} member(s) or content source(s) of this batch are deleted, the \
                         first in artifact {first_artifact}; a deleted member contributes to no \
                         count and makes supplied content unservable from birth, so the batch is \
                         refused rather than published into silence"
                    ),
                },
            ));
        }

        let batch = self.write.publish_artifacts(layer, level, artifacts)?;
        let shard = generation.bundle.manifest.identity.shard_id;
        let tessera_ids = batch
            .entities
            .into_iter()
            .map(|entity| {
                self.identity_key.forward(shard, entity).map_err(|_| {
                    crate::write::AcceptError::Exec(tessera_lifecycle::ExecError::LayerRefused {
                        detail: "an artifact's entity id lies outside the identity space"
                            .to_string(),
                    })
                })
            })
            .collect::<std::result::Result<Vec<_>, _>>()?;
        Ok(PublishedArtifacts {
            tessera_ids,
            created: batch.created,
            without_content: batch.without_content,
            filled: batch.filled,
            joined: batch.joined,
        })
    }

    /// [`Self::put_artifacts`], answering the identifiers alone — the shape every caller that
    /// publishes new artifacts under new keys wants.
    pub fn publish_artifacts(
        &self,
        layer: String,
        level: u32,
        artifacts: Vec<tessera_lifecycle::IncomingArtifact>,
    ) -> std::result::Result<Vec<TesseraId>, crate::write::AcceptError> {
        self.put_artifacts(layer, level, artifacts)
            .map(|published| published.tessera_ids)
    }

    /// Add points to the memberships of artifacts that already exist, each named by the key it was
    /// published under.
    ///
    /// **A build reading a member table has always done this; this is the same operation at the
    /// other entry point** ([decision 0091](../../../docs/decisions/0091-build-is-ingest-into-an-empty-database.md)).
    /// The artifact then behaves exactly as though the point had been there all along: there is no
    /// state in which a cluster holds some of its points because of how they arrived.
    ///
    /// **A suppressed artifact grows like any other and stays suppressed.** The key resolves
    /// against the *store*, never against what is served — so a suppression cannot be defeated by
    /// growing the artifact it hides, and cannot make the growth refuse either
    /// (`artifacts-from-points.md` §5).
    ///
    /// The two member checks are `publish_artifacts`'s, unchanged and for its reasons: a member
    /// with no row would count towards the declared size the proportional criterion divides by
    /// while being visible to nobody, and a **deleted** member can never contribute to a count
    /// again. A **suppressed** member joins: it is a live member temporarily outside every mask.
    ///
    /// **A point may also name its artifacts on the wire**, which is the same operation arriving
    /// with the rows it is about: `/control/ingest` accepts a column named for a declared layer and
    /// grows these memberships inside the batch's own commit window (`artifacts-from-points.md`
    /// §6.2). This entry point stays what an operator uses for a correction against points that are
    /// already there. An unknown key is refused on **this** route whatever the layer's value set
    /// says — see [`tessera_lifecycle::IncomingGrowth`]; the column at `/control/ingest` is where an
    /// open layer creates the artifact a key names.
    ///
    /// The answer is one [`GrownMembership`] per join, in the caller's order: the artifact's
    /// `tessera_id` and how many of the joining members it did not already hold. Neither an
    /// ordinal nor a membership size (C8).
    pub fn grow_memberships(
        &self,
        layer: String,
        level: u32,
        joins: Vec<tessera_lifecycle::IncomingGrowth>,
    ) -> std::result::Result<Vec<GrownMembership>, crate::write::AcceptError> {
        let high_water = self.allocator_high_water() as u32;
        let rowless: u64 = joins
            .iter()
            .map(|j| j.joining.cardinality() - j.joining.range_cardinality(0..high_water))
            .sum();
        if rowless > 0 {
            return Err(crate::write::AcceptError::Exec(
                tessera_lifecycle::ExecError::LayerRefused {
                    detail: format!(
                        "{rowless} of these joining member(s) name no point; a membership is a set \
                         of documents, and a member with no row would count towards the \
                         artifact's declared size while being visible to nobody"
                    ),
                },
            ));
        }

        // A count and the key of the first join naming one, never an entity id (I10): the detail
        // is the caller's 422 body.
        let generation = self.generation();
        let mut deleted = 0u64;
        let mut first_key = None;
        for join in &joins {
            let in_this = join
                .joining
                .iter()
                .filter(|entity| generation.overlay.is_deleted(EntityId::new(*entity as u64)))
                .count() as u64;
            if in_this > 0 {
                deleted += in_this;
                first_key.get_or_insert(join.key.as_str());
            }
        }
        if let Some(first_key) = first_key {
            return Err(crate::write::AcceptError::Exec(
                tessera_lifecycle::ExecError::LayerRefused {
                    detail: format!(
                        "{deleted} joining member(s) are deleted, the first joining '{first_key}'; a \
                         deleted member contributes to no count, so the batch is refused rather \
                         than applied into silence"
                    ),
                },
            ));
        }

        let grown = self.write.grow_memberships(layer, level, joins)?;
        let shard = generation.bundle.manifest.identity.shard_id;
        grown
            .into_iter()
            .map(|receipt| {
                let tessera_id =
                    self.identity_key
                        .forward(shard, receipt.entity)
                        .map_err(|_| {
                            crate::write::AcceptError::Exec(
                                tessera_lifecycle::ExecError::LayerRefused {
                                    detail:
                                        "an artifact's entity id lies outside the identity space"
                                            .to_string(),
                                },
                            )
                        })?;
                Ok(GrownMembership {
                    tessera_id,
                    joined: receipt.joined,
                    filled: receipt.filled,
                    left: receipt.left,
                    withdrawn: receipt.withdrawn,
                })
            })
            .collect()
    }

    /// Which layers this principal may know exist, and which of those are currently served.
    ///
    /// **Two questions, answered in that order, and the order is the disclosure control.**
    /// Reachability is resolved from the registry — one set probe, identical for a gate-failed name
    /// and a never-registered one. What that resolution must *not* carry is the verdict: a
    /// suppression against a layer's own entity takes effect at the ack, so the overlay is asked
    /// live, per call, for every name the resolution admitted. A cache may bake in reachability;
    /// it may never bake in whether a layer is currently served.
    ///
    /// A suppressed layer therefore leaves the resolved set the way a gate-failed one never
    /// entered it — same answer, and by the same route the request path already takes for a point.
    pub fn visible_layers(&self, session: &Session) -> Vec<tessera_types::layer::RegisteredLayer> {
        let generation = self.generation();
        let resolved = self.write.resolve_layers(
            |term| session.satisfied.contains(&term),
            |label| generation.dict.lookup(label.as_bytes()),
        );
        resolved
            .names()
            .filter_map(|name| self.write.registered_layer(name))
            .filter(|layer| {
                // **The live half, and it is asked per call.** A layer's own entity carries its
                // suppression, so this is the same deleted-beats-suppressed composition a point
                // goes through, against the current overlay rather than against whatever was true
                // when the reachable set was resolved. `None` — no opinion — is *not* visible here:
                // a layer has no row and no fragment to fall through to, so the only honest reading
                // of "nothing says yes" is no.
                !generation.overlay.is_deleted(layer.entity)
                    && !generation.overlay.is_suppressed(layer.entity)
            })
            .collect()
    }

    /// One past the lowest row-less entity ever allocated. Operator-facing, beside the point
    /// region's high-water mark on `/control/status`: the two together are how much of the entity
    /// space is left, which neither answers alone.
    pub fn allocator_low_water(&self) -> u64 {
        self.write.allocator_low_water()
    }

    /// Where an artifact's entity sits, and what was published there.
    ///
    /// **Addressing, and the caller gates afterwards.** This says an entity is artifact *n* of a
    /// level; it says nothing about whether the asker may know that, and every route acting on the
    /// answer puts it through the one predicate first. The membership itself is deliberately not
    /// returned — a caller with a raw member set could count it, and an unmasked count over items a
    /// principal may not see is C8's row.
    pub fn locate_artifact(&self, entity: EntityId) -> Option<PublishedArtifactAddress> {
        let (layer, level, ordinal) = self.write.locate_artifact(entity)?;
        let key = self.write.with_artifacts(|store| {
            store
                .get(&layer, level, ordinal)
                .and_then(|r| r.key.clone())
        });
        Some(PublishedArtifactAddress {
            layer,
            level,
            ordinal,
            key,
        })
    }

    /// How many artifacts this node holds, across every layer.
    ///
    /// **Operator-facing, and there is deliberately no per-layer form.** A per-layer count is a
    /// corpus-wide count over objects a principal may not individually see, which is C8's row; the
    /// total answers "is the store populated" for `/control/status` without answering that.
    pub fn published_artifacts(&self) -> usize {
        self.write.with_artifacts(|store| store.total())
    }

    /// How many resident memberships are held on the heap rather than read through the extent
    /// that carries them — see [`tessera_lifecycle::membership::ArtifactStore::owned_memberships`].
    ///
    /// A test hook on [`Self::level_memberships_for_test`]'s argument, and a count rather than a
    /// membership: what it answers is whether a publication left anything behind.
    #[cfg(feature = "fault-injection")]
    #[doc(hidden)]
    pub fn owned_memberships_for_test(&self) -> usize {
        self.write.with_artifacts(|store| store.owned_memberships())
    }

    /// Every artifact of one level as `(ordinal, members, mapped)`, where `mapped` says the
    /// membership is read through the extent that carries it rather than from the heap.
    ///
    /// **A test hook, gated so it cannot exist in a shipped build**, on
    /// [`Self::set_background_refresh_for_test`]'s argument and on a second one of its own: this
    /// answers memberships in entity space with no mask anywhere near it, which is the shape no
    /// serving route may have. Where a membership lives is otherwise invisible — every reader goes
    /// through `Deref` and cannot tell the two apart — so without it the seed and the fold could
    /// stop mapping and only a memory measurement would ever say so.
    #[cfg(feature = "fault-injection")]
    #[doc(hidden)]
    pub fn level_memberships_for_test(
        &self,
        layer: &str,
        level: u32,
    ) -> Vec<(u32, Vec<u32>, bool)> {
        self.write.with_artifacts(|store| {
            store
                .level(layer, level)
                .map(|(ordinal, record)| {
                    (
                        ordinal,
                        record.members.iter().collect::<Vec<u32>>(),
                        record.members.is_mapped(),
                    )
                })
                .collect()
        })
    }
}

/// One join's answer from [`Engine::grow_memberships`].
///
/// `joined` is bounded by the members the caller sent, so it says nothing about the members they
/// did not: a membership size never crosses the boundary (C8).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct GrownMembership {
    /// The artifact's identifier, the same one its publication answered with.
    pub tessera_id: TesseraId,
    /// How many of the joining members were not already in the membership.
    pub joined: u64,
    /// How many of the fixed parts the join carried were absent and are now held
    /// (`ingest.md` §1.5).
    pub filled: u64,
    /// How many of the leaving members a generating-set page took out of it.
    pub left: u64,
    /// The rank this page emptied, where it emptied one: the content is withdrawn and the caller
    /// supplies it again (`ingest.md` §1.1).
    pub withdrawn: Option<u16>,
}

/// What [`Engine::put_artifacts`] answers: the identifiers in the caller's order and the batch's
/// counts (`ingest.md` §1.5), each bounded by the caller's own request.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PublishedArtifacts {
    /// One per artifact in the caller's order: a held artifact's own, or the new one's.
    pub tessera_ids: Vec<TesseraId>,
    /// How many artifacts the batch created; the rest were held.
    pub created: u64,
    /// How many created artifacts carry no content on a layer declaring some (R5).
    pub without_content: u64,
    /// How many fixed parts were filled on held artifacts.
    pub filled: u64,
    /// How many members joined held artifacts that did not already hold them.
    pub joined: u64,
}

/// Where an artifact sits, as [`Engine::locate_artifact`] answers it.
///
/// The ordinal is here because the engine's own routes address by it. **It does not cross the
/// wire** — see `Ack::ArtifactsPublished` for why two ordinals are a count of what lies between.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PublishedArtifactAddress {
    pub layer: String,
    pub level: u32,
    pub ordinal: u32,
    pub key: Option<String>,
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
    let filter_columns = Arc::new(
        crate::filter::FilterColumns::open(
            &prefix_dir,
            &phash,
            &bundle.manifest.declared_scalars,
            &bundle.manifest.scoped_scalars(),
            &|view: &str| bundle.manifest.incarnation_of(view),
            &bundle.manifest.vocabularies,
            &partition.manifest.attr_extents,
            // The record blob rotates with the prefix for the reason the value columns do: the
            // fold rewrites it, and the superseded prefix's files are pre-blanking.
            &partition.manifest.record_extents,
            &partition.manifest.artifact_record_extents,
            &partition.manifest.entity_terms_extents,
            &partition.manifest.text_extents,
            &unfolded_attributes,
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

/// One entity's record-blob row, decompressed **at most once** whatever how many blob-resident
/// columns ask for it (review finding F4).
///
/// `fields_of` decompresses the block the entity's row sits in, and the join rule's attribute arm
/// asks it once per blob-resident column: a schema with six such columns paid six decompressions of
/// one block per joining row. The row is the same for all of them, so it is read here and shared.
/// The outer `Option` is *not yet read*; the inner one is the blob's own answer, which is `None`
/// for an entity with no row and for a blob that could not be read alike — the same collapse
/// [`flushed_scalar_of`] documents, and for the same reason.
#[derive(Default)]
pub(crate) struct BlobRow(Option<Option<Vec<tessera_filter::RecordField>>>);

impl BlobRow {
    fn get(
        &mut self,
        generation: &Generation,
        entity: u32,
    ) -> &Option<Vec<tessera_filter::RecordField>> {
        self.0.get_or_insert_with(|| {
            match generation.filter_columns.records().fields_of(entity) {
                Ok(fields) => fields,
                // **The error's *kind*, never its `Display`** (**I10**). `RecordError::Malformed`
                // carries a detail string, and the blob's detail strings name the entity in six
                // spellings — its addressing checks are about *which* entity's row was found. The
                // byte-scanner sweeps logs as well as payloads, so this warning carries the
                // artefact and the class of defect, which is what an operator chasing a systematic
                // build or flush fault needs; the row that tripped it buys nothing an
                // entity-independent message does not. The drill-down propagates the same error as
                // a refusal, and that path may carry the detail: it reaches an operator's error
                // surface rather than the log the scanner reads.
                Err(e) => {
                    let kind = match &e {
                        tessera_filter::RecordError::Io(io) => io.kind().to_string(),
                        tessera_filter::RecordError::Malformed(_) => "malformed".to_string(),
                    };
                    tracing::warn!(
                        artefact = "attrs/record",
                        kind = %kind,
                        "the record blob could not answer, so the join rule's attribute arm has \
                         nothing to compare a blob-resident column against and this batch's joins \
                         are accepted unchecked (views §4). The artefact is a build or flush \
                         defect; a fold rewrites it."
                    );
                    None
                }
            }
        })
    }
}

/// An already-flushed entity's stored value for one declared column, at the shape a batch carries
/// it in — the join rule's attribute arm and its render backfill, once the entity's own row has
/// left the commit-window buffer (`views.md` §4).
///
/// **The label arm's shape, over three homes instead of one.** `entities/terms/` answers the label
/// question outright; an entity-scoped *value* has no single artefact, so this reads the home the
/// declaration puts it in and there are exactly three (records §3, decision 0068): the entity-space
/// value column where the column owes one (`index = true`, or a `derived` category's floor), the
/// record blob where it is blob-resident (text always; anything neither rendered nor
/// value-columned), and the hot column where `render = true` is the value's only store. The three
/// are exhaustive and, per column, the first that applies is the cheapest — only a render-only
/// column pays a row lookup.
///
/// The answer is returned as a [`tessera_lifecycle::WalScalar`] so the comparison at the call site
/// is the *same* equality the buffered arm makes against a buffered row's scalars: one comparison,
/// two sources, and the two arms cannot come to disagree about what "the same value" means.
///
/// `None` is *no value held*, and it is also *could not find out* — the two are one answer here
/// because the join it guards is inert in entity space either way (a joining row writes no
/// postings, no attribute column and no record field), so what an unreadable artefact costs is the
/// refusal, not the rule. Corruption is warned, never swallowed, and the warning names the artefact
/// rather than the entity (**I10**).
///
/// **A free function rather than a method, because the write executor needs this form**: it must
/// read the same generation its apply will clone from rather than re-loading the pointer under
/// itself. `blob` is the caller's per-row [`BlobRow`], so a schema's blob-resident columns share one
/// decompression.
///
/// **Server-side and control-plane.** Nothing derived from this reaches a client: the refusal names
/// the column, exactly as the buffered arm's does, and never the value on either side.
pub(crate) fn flushed_scalar_of(
    generation: &Generation,
    entity: EntityId,
    declared_index: usize,
    blob: &mut BlobRow,
) -> Option<tessera_lifecycle::WalScalar> {
    let manifest = &generation.bundle.manifest;
    let declared = manifest.declared_scalars.get(declared_index)?;
    let vocabularies = &manifest.vocabularies;
    // I9 caps entity ids at `u32::MAX`, and the same `expect` guards the drill-down's read. A
    // violated invariant is loud rather than a `None` the caller would read as "no value held"
    // and accept a mismatch under.
    let entity_raw = u32::try_from(entity.raw()).expect("entity ids are capped at u32::MAX by I9");

    let stored = if crate::filter::owes_value_column(declared, vocabularies) {
        generation
            .filter_columns
            .stored_value(&declared.name, entity_raw)
    } else if crate::filter::blob_resident(declared, vocabularies) {
        // The blob is keyed by entity and its rows are self-describing, so the field wanted is the
        // one tagged with this column's declared position (records §3). A malformed row refuses on
        // the drill-down path, which propagates it; here it is a lost report — warned once per
        // row by `BlobRow`, which is also what keeps this to one decompression however many
        // blob-resident columns the schema declares.
        blob.get(generation, entity_raw)
            .as_ref()?
            .iter()
            .find_map(|f| (f.tag as usize == declared_index).then(|| f.value.clone()))
    } else {
        crate::viewport::flushed_row_scalar(generation, entity, declared_index)
    }?;
    stored_as_wal(stored, declared)
}

/// [`Engine::flushed_terms`]'s body, over a generation the caller already holds.
///
/// **The write executor needs this form, and that is why it is not a method** — the same reason
/// [`flushed_scalar_of`] is not one. The join rule's label arm runs on the serial writer
/// (decision 0116), beside the generation its apply will clone from, and re-loading the pointer
/// under itself is exactly the race the relocation exists to close.
pub(crate) fn flushed_terms_of(generation: &Generation, entity: EntityId) -> Option<Vec<TermId>> {
    let entity = u32::try_from(entity.raw()).ok()?;
    let terms = match generation.filter_columns.entity_terms().terms_of(entity) {
        Ok(terms) => terms?,
        Err(e) => {
            tracing::warn!(
                error = %e,
                "the entity->term transpose could not answer, so the join rule's label arm \
                 has nothing to compare against and this batch's joins are accepted \
                 unchecked (views §4). The artefact is a build or flush defect and the error \
                 names the file; a fold rewrites it."
            );
            return None;
        }
    };
    Some(terms.into_iter().map(TermId::new).collect())
}

/// One already-flushed `(entity, attribute, key)` cell's value, at the shape a batch carries it in
/// — the scoped half of the join rule's attribute arm (`views.md` §5, decision 0116).
///
/// `owner_view` is the cell's address, `write::scoped_owner_view_of`'s answer, so a value written
/// through a sharing group's door and one written through the owner's are read back from the one
/// column. A family on no filter surface has no store to read and answers `None`, as does a `text`
/// family, whose extent is a dictionary and postings and holds no value per entity; both lose the
/// comparison rather than the rule, exactly as a blob-resident entity-scoped column does.
pub(crate) fn flushed_scoped_of(
    generation: &Generation,
    entity: EntityId,
    family: &tessera_store::manifest::ScopedScalar,
    owner_view: &str,
) -> Option<tessera_lifecycle::WalScalar> {
    if !crate::filter::scoped_is_filterable(family) {
        return None;
    }
    let entity = u32::try_from(entity.raw()).ok()?;
    let column = crate::filter::scoped_column_name(&family.name, owner_view);
    let stored = generation.filter_columns.stored_value(&column, entity)?;
    stored_as_wal(stored, &declared_of_scoped(family))
}

/// Does a **flushed** layer of this `(entity, attribute, key)` cell hold prose?
///
/// The `text` half of the scoped cell arm (`views.md` §5, decision 0116; review finding F1). A text
/// family has no per-entity value for [`flushed_scoped_of`] to answer with, so the cell arm asks
/// occupancy instead of equality and refuses a supplied string where the cell is occupied. `false`
/// for every other family, which has a value to compare, and for a family on no filter surface,
/// which has no column at all.
///
/// ⊘ **The build's base is not covered** — it writes no presence file
/// ([`crate::filter::FilterColumns::text_present`], issue #123) — so a cell whose only prose came
/// from the build reads as unoccupied. That is an under-refusal and it is stated rather than
/// hidden: the fix is the base's presence bitmap, not a change here.
pub(crate) fn flushed_scoped_text_present(
    generation: &Generation,
    entity: EntityId,
    family: &tessera_store::manifest::ScopedScalar,
    owner_view: &str,
) -> bool {
    if !crate::filter::scoped_is_filterable(family) {
        return false;
    }
    let Ok(entity) = u32::try_from(entity.raw()) else {
        return false;
    };
    let column = crate::filter::scoped_column_name(&family.name, owner_view);
    generation.filter_columns.text_present(&column, entity)
}

/// A scoped family as the entity-scoped declaration the absence and comparison helpers take.
///
/// **The same transcription the ingest boundary makes** (`control.rs`'s `scoped_as_declared`): a
/// scoped column *is* an entity-scoped one — same types, same vocabulary, same absence rules — so
/// the two helpers that decide what "absent" and "the same value" mean take one shape and cannot
/// come to mean two things.
pub(crate) fn declared_of_scoped(
    family: &tessera_store::manifest::ScopedScalar,
) -> tessera_store::manifest::DeclaredScalar {
    tessera_store::manifest::DeclaredScalar {
        name: family.name.clone(),
        arrow_type: family.arrow_type,
        vocabulary: family.vocabulary.clone(),
        analyser: family.analyser.clone(),
        index: family.index,
        render: family.render,
    }
}

/// Is this value **no value at all** for `declared` — the join rule's "or be absent from the
/// batch" (`views.md` §4), asked of a supplied value and a stored one alike?
///
/// **Two spellings, because a category's absence is in band.** Every other family says absence
/// with [`tessera_lifecycle::WalScalar::Null`], having no bit pattern to spare; a vocabulary keeps
/// code 0 out of its value space precisely so a category can say it with a code
/// ([`tessera_store::vocabulary::ABSENT_CODE`], per-point-attributes §3.4), and the ingest parse
/// turns a null category cell into that code before either arm sees it. Reading only the `Null`
/// spelling made a joining batch that left a category null a `409` against an entity holding a
/// value, and a join carrying a value against an entity holding *none* a `409` as well, neither of
/// which the rule asks for.
///
/// One definition, because three callers ask it: the handler's comparison, on both sides, and the
/// executor's backfill.
pub(crate) fn scalar_is_absent(
    value: &tessera_lifecycle::WalScalar,
    declared: &tessera_store::manifest::DeclaredScalar,
) -> bool {
    use tessera_lifecycle::WalScalar as WS;
    if matches!(value, WS::Null) {
        return true;
    }
    if declared.vocabulary.is_none() {
        return false;
    }
    let absent = tessera_store::vocabulary::ABSENT_CODE;
    match value {
        WS::U8(c) => u32::from(*c) == absent,
        WS::U16(c) => u32::from(*c) == absent,
        WS::U32(c) => *c == absent,
        // A novel key on a `discovered` vocabulary travels as its key and is minted at the commit
        // window's close; a key is never absence — the empty string is refused upstream.
        _ => false,
    }
}

/// One stored value at the shape an ingest batch carries it in, so the join rule's attribute arm
/// compares like with like whichever home answered (`views.md` §4).
///
/// **Three normalisations, one per family whose storage type is not its wire type**, and each is a
/// storage fact rather than a presentation choice:
///
/// - **A `bool` stores as a byte** in a value column and as an Arrow boolean in the hot column, and
///   arrives as [`WalScalar::Bool`]; non-zero is `true`.
/// - **A `timestamp_us` stores as an `i64`** whose unit the declaration fixes, and arrives as
///   [`WalScalar::TimestampUs`]; the two are the same number.
/// - **A category stays a code**, and is deliberately *not* resolved to its key. The code is what
///   the batch carries by the time this comparison runs (`category_scalar` mints nothing and
///   resolves through the live bindings), the code is what the entity stores, and resolving both
///   ends through a vocabulary would make a rebinding — which never happens, codes being
///   never-reused — the only thing the extra work could ever detect. The declared width is the
///   one both sides are read at, so a `u8` column's code cannot compare unequal to itself because
///   one home widened it.
///
/// Every other family is its own storage type: a number byte-matches a number, and a keyword or a
/// text field matches on its **bytes** — decoded from this layer's sorted dictionary where the
/// value column holds an ordinal (**I10**: an ordinal is an index internal and never the unit of
/// comparison across a flush boundary, because two flushes number the same key differently).
///
/// `None` where the stored value cannot be read at the declared type at all, which is a malformed
/// bundle rather than a caller's error and so loses the report rather than refusing the batch.
fn stored_as_wal(
    value: tessera_filter::RecordValue,
    declared: &tessera_store::manifest::DeclaredScalar,
) -> Option<tessera_lifecycle::WalScalar> {
    use tessera_filter::RecordValue as RV;
    use tessera_lifecycle::WalScalar as WS;
    use tessera_spatial::tiler::ScalarType;

    if declared.vocabulary.is_some() {
        let code = match value {
            RV::U8(c) => u32::from(c),
            RV::U16(c) => u32::from(c),
            RV::U32(c) => c,
            _ => return None,
        };
        return Some(match declared.arrow_type {
            ScalarType::U8 => WS::U8(code as u8),
            ScalarType::U16 => WS::U16(code as u16),
            _ => WS::U32(code),
        });
    }
    Some(match (declared.arrow_type, value) {
        (ScalarType::Bool, RV::U8(x)) => WS::Bool(x != 0),
        (ScalarType::Bool, RV::Bool(b)) => WS::Bool(b),
        (ScalarType::TimestampUs, RV::I64(x)) | (ScalarType::TimestampUs, RV::TimestampUs(x)) => {
            WS::TimestampUs(x)
        }
        (_, RV::U8(x)) => WS::U8(x),
        (_, RV::U16(x)) => WS::U16(x),
        (_, RV::U32(x)) => WS::U32(x),
        (_, RV::U64(x)) => WS::U64(x),
        (_, RV::I8(x)) => WS::I8(x),
        (_, RV::I16(x)) => WS::I16(x),
        (_, RV::I32(x)) => WS::I32(x),
        (_, RV::I64(x)) => WS::I64(x),
        (_, RV::F32(x)) => WS::F32(x),
        (_, RV::F64(x)) => WS::F64(x),
        (_, RV::Bool(b)) => WS::Bool(b),
        (_, RV::TimestampUs(x)) => WS::TimestampUs(x),
        (_, RV::Utf8(s)) => WS::Utf8(s),
        // ⊘ Lists land with epic 3's multi surface; no writer produces one, and a reader that met
        // one would be looking at a future format — no answer, not a guess.
        (_, RV::List(_)) => return None,
    })
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
