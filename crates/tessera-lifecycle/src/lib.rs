//! `tessera-lifecycle` — the WAL and the I9 entity-ID allocator (plan §5, Task 9).
//!
//! Fail-closed durability machinery: an unpersisted deny entry fails open, and the [`wal`]
//! module's positional CRC rule is the difference between ordinary crash recovery and silent
//! loss of acked security state. [`alloc`] is the append-only, never-reusing entity-ID allocator
//! (I9) plus the signature-sorted assignment helper appended items go through. [`overlay`] and
//! [`buffer`] (Task 10) are the replayed WAL's live authorisation-relevant state: the overlay's
//! three independent deny/evaluate facts and the not-yet-built ingest buffer.

//!
//! [`command`] is the Phase 2 write-executor vocabulary (stage 2.1, Task 0b) — landed ahead of
//! its consumers, and **unused until Task 3a**. The executor thread that consumes it lives in
//! `tessera-engine`, not here: only that crate can see both a `Wal` and a `Generation`.

pub mod alloc;
pub mod buffer;
pub mod command;
pub mod faults;
pub mod overlay;
pub mod wal;
pub mod window;

pub use alloc::{assign_sorted, high_water_from, Allocator, PendingItem};
pub use buffer::{BufferedItem, DescriptorResolver, IngestBuffer};
pub use command::{Ack, Command, ExecError, Receipt, SubmitError, UnallocatedRow};
pub use faults::WalMeter;
pub use overlay::{replay, Overlay, OverlayEntry, OverlayError};
pub use wal::{ChangeOp, ExecutorWal, Wal, WalError, WalRecord, WalRow, WalScalar};
pub use window::{ClosedEntry, CommitWindow, WindowEntry};
