//! `tessera-lifecycle` — the write-ahead log and the I9 entity-ID allocator.
//!
//! Fail-closed durability machinery: an unpersisted deny entry fails open, and the [`wal`]
//! module's positional CRC rule is the difference between ordinary crash recovery and silent
//! loss of acked security state. [`alloc`] is the append-only, never-reusing entity-ID allocator
//! (I9) plus the signature-sorted assignment helper appended items go through. [`overlay`] and
//! [`buffer`] are the replayed WAL's live authorisation-relevant state: the overlay's two
//! independent deny facts and the ingest buffer.
//!
//! [`command`] is the write-executor vocabulary and [`window`] the commit window it is gathered
//! into. The executor thread that consumes both lives in `tessera-engine`, not here: only that
//! crate can see both a `Wal` and a `Generation`.
//!
//! [`roster`] is the view roster — which views of which groups exist, and which keys are burnt.
//! It sits beside the layer registry because the two problems are one: a named object created
//! while the service runs, made durable by a record in this log and carried forward for ever by
//! the segments manifest.
//!
//! [`registry`] is the annotation layer registry — what layers exist, what they declared, and who
//! may know it. It lives here rather than in the engine because a layer's identity is durable state
//! recovered by replay, which is exactly what this crate is: its records sit in the same log, its
//! entities come from the same allocator, and its suppressions ride the same overlay.

pub mod alloc;
pub mod buffer;
pub mod command;
pub mod faults;
pub mod membership;
pub mod overlay;
pub mod registry;
pub mod roster;
pub mod wal;
pub mod window;

pub use alloc::{
    allocator_ceiling, allocator_floor, assign_sorted, high_water_from, low_water_from, Allocator,
    PendingItem,
};
pub use buffer::{BufferedItem, DescriptorResolver, IngestBuffer};
pub use command::{
    Ack, AttributeRequest, BatchArtifacts, BatchEdge, BatchMembership, Command, ExecError,
    MembershipGrown, Receipt, SubmitError, UnallocatedRow,
};
pub use faults::WalMeter;
pub use membership::{ArtifactRecord, ArtifactStore, IncomingArtifact, IncomingGrowth, Members};
pub use overlay::{owner_id_only, replay, Overlay};
pub use registry::{no_pending, EdgeCheck, LayerRegistry, RegistryError, ResolvedLayers};
pub use roster::{GroupFacts, RosterError, ViewRoster};
pub use wal::{
    ChangeOp, ExecutorWal, MembershipGrowth, OverlaySnapshotEntry, PublishedArtifact, Wal,
    WalError, WalRecord, WalRow, WalScalar,
};
pub use window::{ClosedEntry, CommitWindow, ResolvedMembership, WindowEntry};
