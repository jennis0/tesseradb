//! `mosaica-server`'s viewer/session/control handlers move their engine calls into
//! `tokio::task::spawn_blocking`, whose closure bound is `'static + Send`. That is what makes
//! `Engine: Send + Sync` load-bearing — every closure
//! captures a cloned `Arc<AppState>` (or an `Arc<SessionEntry>` wrapping a `Session`), and an
//! `Arc<T>` is `Send` only if `T: Send + Sync`.
//!
//! This is a compile-only check, deliberately: if any of these four types stopped being
//! `Send + Sync`, `assert_send_sync::<T>()` below fails to compile, not to run — exactly the
//! guarantee `spawn_blocking`'s own bound already enforces at every call site in
//! `mosaica-server`, made explicit here as a standing assertion over the types themselves rather
//! than an incidental consequence of how one call site happens to use them.
//!
//! The compiler derives both traits for these types from their fields, and the workspace's only
//! `unsafe impl` of either beneath them is the pair on `mosaica_roaring::OwnedView`.

use mosaica_authz::FrozenFragment;
use mosaica_engine::{Engine, Session};
use mosaica_store::SegmentData;

fn assert_send_sync<T: Send + Sync>() {}

#[test]
fn engine_session_fragment_and_segment_data_are_send_and_sync() {
    assert_send_sync::<Engine>();
    assert_send_sync::<Session>();
    assert_send_sync::<FrozenFragment>();
    assert_send_sync::<SegmentData>();
}
