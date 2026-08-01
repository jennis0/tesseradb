//! `tessera-lifecycle` — the write-ahead log and the I9 entity-ID allocator.
//!
//! Fail-closed durability machinery: an unpersisted deny entry fails open, and the [`wal`]
//! module's positional CRC rule is the difference between ordinary crash recovery and silent
//! loss of acked security state. [`alloc`] is the append-only, never-reusing entity-ID allocator
//! (I9) plus the signature-sorted assignment helper appended items go through. [`overlay`] and
//! [`buffer`] are the replayed WAL's live authorisation-relevant state: the overlay's three
//! independent deny/evaluate facts and the ingest buffer.
//!
//! [`command`] is the write-executor vocabulary and [`window`] the commit window it is gathered
//! into. The executor thread that consumes both lives in `tessera-engine`, not here: only that
//! crate can see both a `Wal` and a `Generation`.

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
