//! The pin seam (I11, lifecycle §2.3).
//!
//! Carved out of `viewport.rs` (Phase 2 stage 2.1, Task 0a). Today [`PinManager`] holds its two
//! bounds and nothing else, and [`PinManager::resolve`] is the equality check moved verbatim from
//! the viewport path; the drain list of superseded generations arrives at Task 4. What the carve
//! buys now is [`PinnedGeometry`] — read its doc before changing anything here, because the type
//! exists to make a specific fail-open mistake fail to compile.

use std::sync::Arc;

use tessera_store::Bundle;
use tessera_types::PinId;

use crate::session::EngineError;
use crate::Generation;

/// Resolves a request's presented pin (or mints one) against the live generation, within
/// lifecycle §2.2's two bounds.
///
/// **Its inputs are Task 4's, not this commit's** *(Task 0 gate, F3)*. Task 4 must deliver
/// `a_pin_past_its_ttl_is_410` and `a_session_cannot_exceed_its_pin_cap`, and neither was
/// expressible against the carved signature: `resolve` was handed no session identity, so "this
/// session's pins" named nothing, and the constructor took no bounds, so `pin_ttl_secs` and
/// `pins_per_session_max` had no way in. Both arrive here now, **accepted and unused**, so that
/// Task 4's diff is a change of behaviour rather than a change of signature — which plan rule 4
/// freezes at Task 0 review precisely because Track C is independent of Track B only while these
/// signatures hold.
pub(crate) struct PinManager {
    /// Lifecycle §2.2's TTL, in seconds. Task 4 expires a drain entry past it; nothing reads it
    /// yet. See `tessera-server::config`'s `DEFAULT_PIN_TTL_SECS` for the page-cache argument that
    /// sizes it.
    pin_ttl_secs: u64,
    /// Lifecycle §2.2's per-session cap. Task 4 refuses a mint above it with
    /// [`EngineError::PinCapExceeded`] — the variant and its 422 mapping are landed by the seam
    /// (see that variant's doc), so Track C needs no edit to `tessera-server/src/error.rs`, which
    /// Track B owns.
    pins_per_session_max: usize,
}

impl PinManager {
    pub(crate) fn new(pin_ttl_secs: u64, pins_per_session_max: usize) -> Self {
        PinManager {
            pin_ttl_secs,
            pins_per_session_max,
        }
    }

    /// Today: the equality check moved verbatim from viewport.rs. Task 4 adds the drain list, the
    /// TTL and the per-session cap.
    ///
    /// I11 / lifecycle §2.3: a pin is geometry identity only — `(prefix, segments_version)` —
    /// never `overlay_version`. An overlay swap (any accepted suppression/delete/predicate
    /// change) must not invalidate a pin; only a bundle swap (new `prefix`/`segments_version`)
    /// does. `None` mints a pin naming the live generation's geometry.
    ///
    /// `token_id` is the session identity Task 4's per-session cap counts against — the same
    /// process-local id the row-projection cache keys on, already in hand at the call site.
    /// **Unused in this commit**, deliberately: this method's behaviour is byte-for-byte what it
    /// was, and the seam commit changes no behaviour (plan rule 1).
    pub(crate) fn resolve(
        &self,
        presented: Option<PinId>,
        token_id: u64,
        live: &Arc<Generation>,
    ) -> Result<PinnedGeometry, EngineError> {
        // The three Task 4 inputs, discarded in one place rather than each being renamed with a
        // leading underscore: an underscore would have to come back off at Task 4, which is
        // exactly the signature churn F3 exists to avoid. When this line disappears, the bounds
        // have a consumer.
        let _task_4_inputs = (token_id, self.pin_ttl_secs, self.pins_per_session_max);

        if let Some(presented) = presented {
            if presented.prefix != live.prefix
                || presented.segments_version != live.segments_version
            {
                return Err(EngineError::PinExpired);
            }
        }
        Ok(PinnedGeometry {
            prefix: live.prefix.clone(),
            segments_version: live.segments_version,
            watermark: live.watermark,
            bundle: Arc::clone(&live.bundle),
        })
    }
}

/// Geometry only. Overlay, buffer and overlay_version are deliberately ABSENT: a pin fixes
/// row-space geometry and NEVER authorisation state (I11, lifecycle §2.3) — a suppression
/// applies to a pinned request the moment it is accepted. Returning a whole `Generation` here
/// is the natural implementation and it is fail-open: the request would compose against a
/// pre-suppression overlay. Task 4 introduces a drain list of superseded generations, at which
/// point that mistake becomes available; this type is what makes it not compile.
pub(crate) struct PinnedGeometry {
    pub prefix: String,
    pub segments_version: u64,
    /// ADVISORY — status and debugging only. NEVER an input to I1 composition: the effective
    /// watermark is always the mask fragment's own (lifecycle §2.3 and its Appendix R action 2,
    /// which amended five separate phrasings implying otherwise).
    // Unread by design, hence the allow: the composition path takes its watermark from the mask
    // fragment, never from here, and the day this field acquires a reader is the day that rule
    // wants re-checking. Deleting the field instead would lose the warning above with it.
    #[allow(dead_code)]
    pub watermark: u64,
    pub bundle: Arc<Bundle>,
}

impl PinnedGeometry {
    /// The wire pin naming this geometry — `(prefix, segments_version)` and nothing else.
    ///
    /// Here rather than at the call site because `viewport.rs` was re-deriving it field by field
    /// from the two public fields above (Task 0 gate, F3), which is a `PinId` this type can no
    /// longer keep honest: the day a pin gains a third component, one construction site changes
    /// instead of one per viewer verb. `watermark` is **not** part of it — contracts §2.6 makes
    /// that component advisory, and a pin the client round-trips must name geometry alone.
    pub(crate) fn pin_id(&self) -> PinId {
        PinId {
            prefix: self.prefix.clone(),
            segments_version: self.segments_version,
        }
    }
}
