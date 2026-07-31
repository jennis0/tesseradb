//! The pin seam (I11, lifecycle §2.3).
//!
//! Carved out of `viewport.rs` (Phase 2 stage 2.1, Task 0a). Today [`PinManager`] holds no state
//! and [`PinManager::resolve`] is the equality check moved verbatim from the viewport path; the
//! drain list of superseded generations arrives later. What the carve buys now is
//! [`PinnedGeometry`] — read its doc before changing anything here, because the type exists to
//! make a specific fail-open mistake fail to compile.

use std::sync::Arc;

use tessera_store::Bundle;
use tessera_types::PinId;

use crate::session::EngineError;
use crate::Generation;

/// Resolves a request's presented pin (or mints one) against the live generation.
///
/// Empty in this commit: the pin check it carries needs nothing but the live generation, and the
/// state a pin manager will eventually own (the drain list of superseded generations a pinned
/// request may still read geometry from) does not exist yet. It is a type rather than a free
/// function so that state has somewhere to land without every call site changing again.
pub(crate) struct PinManager {}

impl PinManager {
    pub(crate) fn new() -> Self {
        PinManager {}
    }

    /// Today: the equality check moved verbatim from viewport.rs. Task 4 adds the drain list.
    ///
    /// I11 / lifecycle §2.3: a pin is geometry identity only — `(prefix, segments_version)` —
    /// never `overlay_version`. An overlay swap (any accepted suppression/delete/predicate
    /// change) must not invalidate a pin; only a bundle swap (new `prefix`/`segments_version`)
    /// does. `None` mints a pin naming the live generation's geometry.
    pub(crate) fn resolve(
        &self,
        presented: Option<PinId>,
        live: &Arc<Generation>,
    ) -> Result<PinnedGeometry, EngineError> {
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
