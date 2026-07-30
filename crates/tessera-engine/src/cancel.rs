//! D-C: cooperative cancellation for the viewport path (the rapid-pan case).
//!
//! [`CancelToken`] is a newtype over `Arc<AtomicBool>` — deliberately no `tokio` dependency, so
//! the engine API stays synchronous (lifecycle §7). `tessera-server` mints one per
//! `/v1/viewport` request, moves a clone into the `spawn_blocking` closure, and flips the
//! original from a drop-guard wired to client disconnect (see `tessera-server::viewer`'s
//! `CancelGuard`). [`crate::viewport::Engine::viewport`] polls it at a few checkpoints (its own
//! doc lists them) and aborts the whole request with [`crate::EngineError::Cancelled`] the moment
//! it observes the flip — never a partial `ViewportOut` (I13).
//!
//! **Ordering: `Relaxed` for both the flip and the check.** The flag carries no payload — nothing
//! else needs to be published alongside it or synchronised against it — so `Release`/`Acquire`
//! would buy ordering guarantees this token has no use for, and neither ordering bounds *when*
//! another thread observes the flip, only what other memory operations are ordered relative to
//! it (there are none here). `Relaxed` gives exactly the guarantee this mechanism needs — every
//! thread holding a clone eventually observes a flip made on any other clone — at the lowest cost
//! per check, which matters because the per-tile check sits on the hot path. This is the ordering
//! the design brief explicitly permits ("Relaxed or Acquire load per check is fine").

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;

/// A cheaply-cloneable cooperative-cancellation flag. Every clone shares the same underlying
/// `AtomicBool` via the `Arc`, so flipping any one clone is visible (eventually, per the module
/// doc's ordering note) to every other.
#[derive(Debug, Clone)]
pub struct CancelToken(Arc<AtomicBool>);

impl CancelToken {
    /// A fresh, not-yet-cancelled token.
    pub fn new() -> Self {
        CancelToken(Arc::new(AtomicBool::new(false)))
    }

    /// Flip the flag. Idempotent — cancelling an already-cancelled token (via this handle or any
    /// clone) has no further effect.
    pub fn cancel(&self) {
        self.0.store(true, Ordering::Relaxed);
    }

    /// Has this token — this handle or any clone of it — been cancelled.
    pub fn is_cancelled(&self) -> bool {
        self.0.load(Ordering::Relaxed)
    }
}

impl Default for CancelToken {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_fresh_token_is_not_cancelled() {
        assert!(!CancelToken::new().is_cancelled());
    }

    #[test]
    fn cancelling_one_clone_is_visible_on_another() {
        let token = CancelToken::new();
        let clone = token.clone();
        assert!(!clone.is_cancelled());
        token.cancel();
        assert!(
            clone.is_cancelled(),
            "the flip must be visible through every clone"
        );
    }

    #[test]
    fn cancelling_twice_is_a_harmless_no_op() {
        let token = CancelToken::new();
        token.cancel();
        token.cancel();
        assert!(token.is_cancelled());
    }
}
