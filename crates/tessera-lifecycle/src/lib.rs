//! `tessera-lifecycle` — the WAL and the I9 entity-ID allocator (plan §5, Task 9).
//!
//! Fail-closed durability machinery: an unpersisted deny entry fails open, and the [`wal`]
//! module's positional CRC rule is the difference between ordinary crash recovery and silent
//! loss of acked security state. [`alloc`] is the append-only, never-reusing entity-ID allocator
//! (I9) plus the signature-sorted assignment helper appended items go through.

pub mod alloc;
pub mod wal;

pub use alloc::{assign_sorted, high_water_from, Allocator, PendingItem};
pub use wal::{ChangeOp, Wal, WalError, WalRecord, WalRow, WalScalar};
