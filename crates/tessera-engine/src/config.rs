//! The engine's configuration and the policies a pass reads from it.

#[cfg(doc)]
use crate::engine::Engine;
#[cfg(doc)]
use crate::error::EngineError;

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
    /// all sit above θ serves nothing. [`Engine::open`] refuses it ([`MIN_K_MIN`]).
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
    /// failure looks like a policy that is simply never triggered. [`Engine::open`] refuses it
    /// ([`MIN_SELECTION_WIDTH`]).
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
    /// [`Self::tier_width`]'s reason: below 2 the pass is silently disabled, and
    /// [`Engine::open`] refuses it.
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

/// The smallest `k_min`. At 0 a tile whose visible items all sit above the threshold draws nothing.
pub const MIN_K_MIN: usize = 1;

/// The smallest `tier_width` and `coalesce_width`. Below it a pass never selects anything.
pub const MIN_SELECTION_WIDTH: usize = 2;

impl EngineConfig {
    /// Refuses the values that would leave a viewer a blank tile or stop maintenance silently.
    pub(crate) fn check(&self) -> crate::error::Result<()> {
        if self.k_min < MIN_K_MIN {
            return Err(crate::error::EngineError::ConfigRefused(format!(
                "k_min is {}; set it to {MIN_K_MIN} or more so a non-empty tile always draws a mark",
                self.k_min
            )));
        }
        for (key, width) in [
            ("tier_width", self.tier_width),
            ("coalesce_width", self.coalesce_width),
        ] {
            if let Some(width) = width.filter(|w| *w < MIN_SELECTION_WIDTH) {
                return Err(crate::error::EngineError::ConfigRefused(format!(
                    "{key} is {width}; set it to {MIN_SELECTION_WIDTH} or more, or leave it unset"
                )));
            }
        }
        Ok(())
    }
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
pub(crate) fn merge_policy(config: &EngineConfig) -> tessera_store::merge::MergePolicy {
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
pub(crate) fn coalesce_policy(config: &EngineConfig) -> crate::coalesce::CoalescePolicy {
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
