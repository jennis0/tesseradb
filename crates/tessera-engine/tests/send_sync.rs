//! Task 3 (D-A) acceptance item: `tessera-server`'s viewer/session/control handlers now move
//! their engine calls into `tokio::task::spawn_blocking`, whose closure bound is `'static +
//! Send`. That makes `Engine: Send + Sync` load-bearing for the first time — every closure
//! captures a cloned `Arc<AppState>` (or an `Arc<SessionEntry>` wrapping a `Session`), and an
//! `Arc<T>` is `Send` only if `T: Send + Sync`.
//!
//! This is a compile-only check, deliberately: if any of these four types stopped being
//! `Send + Sync`, `assert_send_sync::<T>()` below fails to compile, not to run — exactly the
//! guarantee `spawn_blocking`'s own bound already enforces at every call site in
//! `tessera-server`, made explicit here as a standing assertion over the types themselves rather
//! than an incidental consequence of how one call site happens to use them.
//!
//! **No `unsafe impl` backs any of this.** `Engine`, `Session`, `FrozenFragment` and `SegmentData`
//! are all Send + Sync automatically (the compiler's auto-trait derivation), because every field
//! they and their transitive dependencies hold is itself `Send + Sync`: `Arc<T: Send + Sync>`,
//! `std::sync::Mutex<T: Send>`, `arc_swap::ArcSwap<T: Send + Sync>`, `memmap2::Mmap` (which is
//! `Send + Sync` by the crate's own design), and `arrow::buffer::Buffer` (whose custom
//! `mmap`-backed allocation in `tessera-store::read::ColumnsRef::load` is built from a `NonNull`
//! wrapped inside `Buffer::from_custom_allocation`, itself `Send + Sync`, not a bare pointer
//! field on `ColumnsRef`/`SegmentData` that would need its own unsafe impl). Grepping the
//! workspace for `unsafe impl` at the time this test was added returns no results — see this
//! task's report.

use tessera_authz::FrozenFragment;
use tessera_engine::{Engine, Session};
use tessera_store::SegmentData;

fn assert_send_sync<T: Send + Sync>() {}

#[test]
fn engine_session_fragment_and_segment_data_are_send_and_sync() {
    assert_send_sync::<Engine>();
    assert_send_sync::<Session>();
    assert_send_sync::<FrozenFragment>();
    assert_send_sync::<SegmentData>();
}
