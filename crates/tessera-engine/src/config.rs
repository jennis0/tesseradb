//! The engine's configuration and the policies a pass reads from it.

#[cfg(doc)]
use crate::engine::Engine;
#[cfg(doc)]
use crate::error::EngineError;

/// Engine-wide configuration: the subset of `tessera.toml`'s `[disclosure]`/`[serve]` sections
/// the engine itself reads. `tessera-server` parses the file; this is the shape [`Engine::open`]
/// consumes.
#[derive(Debug, Clone, Copy)]
pub struct EngineConfig {
    /// How long a freshly minted session token remains valid, in seconds.
    pub token_max_lifetime_secs: u64,
    /// The hard cap on a viewport request's `k`. [`Engine::viewport`] clamps to this defensively
    /// even though the server is expected to enforce it too. A machine ceiling (GPU, transport,
    /// handle table), not the overplot ceiling [`Self::k_max_marks`] is.
    pub max_k: usize,
    /// The minimum marks a non-empty tile draws, whatever the threshold says. Stops a viewer with
    /// few visible items getting a blank map. Must be at least [`MIN_K_MIN`]; [`Engine::open`]
    /// refuses a lower value.
    pub k_min: usize,
    /// The most marks any one tile draws: an overplot ceiling sized from ink coverage per tile,
    /// not the machine ceiling [`Self::max_k`] is. A client may request `k <= k_max_marks`; the
    /// effective cap is the smaller of the two.
    pub k_max_marks: usize,
    /// θ's anchor target: the marks the mean occupied tile should draw at any depth, from which
    /// θ_0 is derived and progressed `x4` per depth. Above a session's total visible count it
    /// saturates, serving every visible row up to the cap.
    pub theta_target_marks: u64,
    /// The largest `underlay_offset` a request may ask for. The sub-cell depth is `zoom + offset`,
    /// clamped to 16.
    pub max_underlay_offset: u8,
    /// The most tiles one `/v1/viewport` may span: an availability bound, see
    /// [`EngineError::TooManyTiles`].
    pub max_tiles_per_request: usize,
    /// The hard ceiling on sub-cells in one response. The underlay multiplies the already-bounded
    /// tile set (see [`Self::max_tiles_per_request`]) by `4^offset`.
    pub max_underlay_cells: usize,
    /// The size of `Engine::open`'s single shared `rayon::ThreadPool`, which every admitted
    /// request's tile loop installs onto. Sized to fill the machine; see
    /// [`default_compute_threads`]. A `0` falls back to rayon's own default rather than a pool
    /// that can run nothing.
    pub compute_threads: usize,
    /// The flush tick, in seconds: the period at which geometry is published. Bounded from below
    /// by the background refresh: a tick shorter than one refresh round leaves every session
    /// permanently in the stale-serve window. See `crate::refresh`.
    pub flush_max_age_secs: u64,
    /// The flush's row trigger: buffered rows at which the tick comes due early. Bounds the
    /// per-row cost of a fast loader, since every commit-window close deep-copies the buffer;
    /// `flush_max_age_secs` bounds how stale a slow one's rows may be.
    pub flush_max_items: usize,
    /// The largest total one row-space merge may consume, and therefore the ceiling on how far
    /// tiering can go before segments stop being mergeable. `None` keeps the built-in default; see
    /// [`default_merge_policy`].
    pub max_merged_segment_bytes: Option<u64>,
    /// How many adjacent, same-tier segments select a row-space merge. `None` keeps the built-in
    /// default (4). Must be at least [`MIN_SELECTION_WIDTH`] when set: below that a merge never
    /// selects. [`Engine::open`] refuses a lower value.
    pub tier_width: Option<usize>,
    /// Sizes at or below this compare equal for merge selection, so a tail of tiny flush segments
    /// forms one tier. `None` keeps the built-in default (16 MiB); see [`merge_policy`].
    pub segment_floor_bytes: Option<u64>,
    /// How many same-tier entries, per axis, select an entity-space coalesce. `None` keeps the
    /// built-in default (8), see [`crate::coalesce::CoalescePolicy`]. Must be at least
    /// [`MIN_SELECTION_WIDTH`] when set, for [`Self::tier_width`]'s reason.
    pub coalesce_width: Option<usize>,
    /// When a fold is dispatched with nobody asking for one. [`CompactionSchedule::off`] is the
    /// value an embedder gets unless it says otherwise: starting a fold, minutes to hours of IO,
    /// from a default nobody chose is not a decision this type may make on a caller's behalf.
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

/// The row-space merge's policy, with every configured knob applied.
///
/// Defaults: `tier_width` 4, floor 16 MiB, cap 256 MiB. The floor is where per-segment overheads
/// stop dominating, so flushes a few bytes apart still merge; the cap bounds one merge's pool
/// time and write amplification. The base segment has no extent, so `plan_merge`'s selection over
/// the extent list can never choose it regardless of these knobs.
pub(crate) fn merge_policy(config: &EngineConfig) -> tessera_store::merge::MergePolicy {
    tessera_store::merge::MergePolicy {
        tier_width: config.tier_width.unwrap_or(4),
        segment_floor_bytes: config.segment_floor_bytes.unwrap_or(16 << 20),
        max_merged_segment_bytes: config.max_merged_segment_bytes.unwrap_or(256 << 20),
    }
}

/// The entity-space coalesce's policy, with `EngineConfig::coalesce_width` applied when set. Only
/// the width is configurable; the floor and the input cap keep
/// [`crate::coalesce::CoalescePolicy`]'s defaults.
pub(crate) fn coalesce_policy(config: &EngineConfig) -> crate::coalesce::CoalescePolicy {
    let mut policy = crate::coalesce::CoalescePolicy::default();
    if let Some(width) = config.coalesce_width {
        policy.width = width;
    }
    policy
}

/// Fill the machine: the same default and fallback (`1`) as the server's own, kept as a free
/// function so every non-server construction site gets the same behaviour.
pub fn default_compute_threads() -> usize {
    std::thread::available_parallelism().map_or(1, std::num::NonZeroUsize::get)
}
