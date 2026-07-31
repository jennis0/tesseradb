//! The lifecycle command vocabulary: what a handler submits to the write executor, and what it
//! gets back (Phase 2 stage 2.1, Task 0b).
//!
//! **Nothing in this module is used yet.** It is landed by the seam commit so that Task 3a
//! implements against a shape frozen at review rather than one invented mid-stream, and so that
//! Task 7a can widen allocation from per-command to per-window without the vocabulary changing
//! underneath it (stage-2.1 plan, "How the work parallelises", rule 4).
//!
//! ## Where the executor lives, and why the vocabulary lives here
//!
//! The executor thread itself is **not** in this crate — it is in `tessera-engine`'s `write.rs`
//! (plan Decision 1). Its loop is append → fsync → **apply → swap** → ack, and apply/swap clone
//! the `IngestBuffer`/`Overlay` out of a `Generation`, which holds a `tessera_store::Bundle`;
//! `tessera-engine` depends on this crate, so a thread here importing `Generation` is a cycle
//! cargo refuses, and this crate deliberately has no `tessera-store` dependency. Everything in
//! this module is entity-space and store-free, which is exactly the half that *can* live here.
//!
//! ## Why the commands carry unallocated rows
//!
//! [`crate::wal::WalRow`]'s `entity_id` is mandatory, so a command carrying `WalRow`s would force
//! the **handler** to allocate before submitting. That makes Task 7a's window-scoped allocation
//! impossible and its headline test — four 25-row submissions producing the same assignment as
//! one 100-row submission — unpassable, because by the time the four submissions reach the window
//! their IDs are already fixed. So allocation happens on the executor, and this type is what a
//! handler submits: everything a `WalRow` needs *except* the ID, plus the resolved term set that
//! is the item's sort signature.
//!
//! The type does not change between Task 3a (allocate per command) and Task 7a (allocate per
//! window). That is the point of landing it now.

use tessera_types::{EntityId, TermId};

use crate::alloc::{AllocError, PendingItem};
use crate::wal::{ChangeOp, WalError, WalRow, WalScalar};

/// One ingest row awaiting entity-ID assignment on the executor.
///
/// Field-for-field [`WalRow`] minus `entity_id`, plus `terms`. Both halves matter:
///
/// - `descriptors` are the **raw descriptor bytes**, carried because that is what the WAL record
///   stores — term IDs are bundle-relative ordinals, and a term coined between builds has no
///   durable ID at all, so a `WalRow` cannot be framed from `terms` alone. (The plan's sketch of
///   this type omitted `descriptors`; without it the executor cannot build the record it is
///   supposed to append. See the Task 0b report.)
/// - `terms` are the **already-resolved** `TermId`s, and they are here because signature-sorted
///   assignment (I9, design §11.1) needs each item's resolved term set to compute its sort key
///   *before* any ID exists. Resolution therefore happens in the handler, ahead of the durability
///   boundary — a structural exception argued at `WritePath::resolve_terms` in `tessera-engine`,
///   not an oversight to be tidied up by moving it onto the executor.
///
/// `external_id` is optional (contracts §3.4 r6) and `None` must never collide with `None`: an
/// item with no external ID is addressable only by its `tessera_id`, is established in no live
/// map, and is not a duplicate of any other such item.
#[derive(Debug, Clone, PartialEq)]
pub struct UnallocatedRow {
    pub external_id: Option<Vec<u8>>,
    pub descriptors: Vec<Vec<u8>>,
    pub x: f32,
    pub y: f32,
    pub scalars: Vec<WalScalar>,
    pub terms: Vec<TermId>,
}

impl UnallocatedRow {
    /// The allocator's view of this row: `(external_id, terms)`, with no ID yet.
    ///
    /// Borrowing rather than consuming, because the rows must survive the allocation that
    /// [`crate::assign_sorted`] performs over the `PendingItem`s — the IDs come back by position
    /// and are then zipped onto the rows by [`UnallocatedRow::into_wal_row`].
    pub fn to_pending(&self) -> PendingItem {
        PendingItem {
            external_id: self.external_id.clone(),
            terms: self.terms.clone(),
            entity_id: None,
        }
    }

    /// The WAL's view of this row, once the executor has assigned `entity_id`.
    ///
    /// Consuming, so the row's heap (`descriptors`, `scalars`, `external_id`) moves into the
    /// record rather than being cloned into it — the framing step is per row per window and the
    /// window is sized in the thousands.
    ///
    /// Deliberately paired with [`UnallocatedRow::to_pending`] in one place: these two are the
    /// whole of the "mechanical conversion" this type exists to make mechanical, and a field
    /// added to [`WalRow`] without a matching field here is a compile error at exactly this
    /// method rather than a silently-dropped column somewhere downstream.
    pub fn into_wal_row(self, entity_id: EntityId) -> WalRow {
        WalRow {
            external_id: self.external_id,
            entity_id,
            descriptors: self.descriptors,
            x: self.x,
            y: self.y,
            scalars: self.scalars,
        }
    }
}

/// One unit of work for the write executor.
///
/// `Flush` and `Compact` are added **additively** in stages 2.2 and 2.3 — new variants, not a
/// changed shape for these two.
#[derive(Debug, Clone, PartialEq)]
pub enum Command {
    /// An accepted `/control/ingest` batch, rows not yet allocated (see [`UnallocatedRow`]).
    ///
    /// `batch_id`/`body_hash` are the idempotency key material and travel with the command
    /// because the batch-state lookup is evaluated **on the executor, not in the handler**
    /// (plan Task 8): between a handler check and the enqueue an open window can close, and a
    /// retry that saw `Unknown` and then enqueued into a fresh window has double-allocated.
    Ingest {
        rows: Vec<UnallocatedRow>,
        batch_id: String,
        body_hash: [u8; 32],
    },
    /// One accepted `/control/changes` entry.
    ///
    /// `entity` is resolved by the handler (from `external_id`, via the live map and the bundle
    /// sidecar) and carried, so the executor does not re-resolve inside its critical section.
    /// `descriptors` carries the new predicate's raw term descriptors for
    /// [`ChangeOp::Predicate`] and is `None` for the three disposition ops, which change
    /// disposition without touching terms.
    Change {
        external_id: Vec<u8>,
        entity: EntityId,
        op: ChangeOp,
        descriptors: Option<Vec<Vec<u8>>>,
    },
}

impl Command {
    /// Whether this command rides the **never-refused** lane (lifecycle §1.3's deny priority
    /// lane): the unbounded queue the executor drains to empty before it touches work.
    ///
    /// The lane is chosen by **endpoint, not by op**. Every `/control/changes` entry takes it,
    /// including [`ChangeOp::Unsuppress`] and [`ChangeOp::Predicate`], because the property being
    /// preserved is contracts §3.1's: `/control/changes` cannot answer 429. Batching a security
    /// operation for latency is acceptable; refusing one for load is not — and an `Unsuppress`
    /// shed for load leaves an item hidden that a caller was told to expect back, which is a
    /// different failure but not a better one.
    ///
    /// Two consequences to choose rather than discover (plan Task 3a): a sustained deny flood
    /// starves ingest completely, and this queue is unbounded in memory.
    pub fn is_never_shed(&self) -> bool {
        matches!(self, Command::Change { .. })
    }
}

/// Why a command could not be **handed to** the executor. Distinct from [`ExecError`], which is
/// why an accepted command failed while executing: this one means nothing was attempted.
///
/// Both variants must be surfaced. A handle that swallows a dead executor while still returning
/// 202s is the worst available outcome — the caller believes its write is in flight and it is
/// not, which for a suppression means an item stays visible with an acknowledgement in hand.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SubmitError {
    /// The bounded work queue is full → HTTP 429 with `Retry-After: retry_after_s` (contracts
    /// §3.1's 429 row).
    ///
    /// **Reachable only from the ingest lane.** A command for which
    /// [`Command::is_never_shed`] holds is submitted to the unbounded queue and can never
    /// produce this variant; Task 6's `changes_never_429s` is the test that asserts it.
    QueueFull { retry_after_s: u64 },
    /// The executor thread is gone (panicked, or shut down) → HTTP 503, and the planes report
    /// not-ready.
    ///
    /// Reachable from **both** lanes: a deny is never refused for *load*, which is not the same
    /// as never refused. There is no honest 200 to give when there is nothing left to apply it.
    ExecutorDead,
}

impl std::fmt::Display for SubmitError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            SubmitError::QueueFull { retry_after_s } => write!(
                f,
                "write queue full — retry after {retry_after_s}s (denies are never shed for load; \
                 this can only be an ingest)"
            ),
            SubmitError::ExecutorDead => {
                write!(
                    f,
                    "the write executor is not running; nothing was submitted"
                )
            }
        }
    }
}

impl std::error::Error for SubmitError {}

/// What the executor did, once it did it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Ack {
    /// One `EntityId` per submitted row, **in the caller's submitted row order** — not in the
    /// signature-sorted order the IDs were assigned in. The handler turns each into a
    /// `tessera_id` for the response (contracts §3.4 r6), which is the only reason an entity ID
    /// is materialised outside the engine at all (I10).
    Ingested { entity_ids: Vec<EntityId> },
    /// A disposition or predicate change applied. Nothing to return: the caller named the item.
    Changed,
}

/// Why an accepted command failed while executing. See [`SubmitError`] for the "never started"
/// cases.
#[derive(Debug)]
pub enum ExecError {
    /// WAL append or fsync failed → HTTP 500 and an alarm.
    ///
    /// **This does not mean "nothing happened".** For [`ChangeOp::Delete`] and
    /// [`ChangeOp::Suppress`] the change is applied to the in-memory overlay and swapped in
    /// *anyway* before the error is returned (lifecycle §4) — never a refusal that leaves a deny
    /// unapplied — so this error means "in force, but not durable", and the operator response is
    /// to treat the WAL as the problem rather than to retry the suppression. For every other
    /// command nothing is applied. The op is the caller's own, so the caller can tell which case
    /// it is in.
    Wal(WalError),
    /// Entity-ID assignment refused (I9's `u32` ceiling) → HTTP 500, fail closed. The batch has
    /// no effect: `Allocator::allocate` leaves the high-water mark unchanged on this path.
    Alloc(AllocError),
    /// This `batch_id` was already accepted, or is held in an open window, with **different**
    /// body bytes → HTTP 409 (contracts §3.4). The retry has no effect, and — Task 8, open
    /// question O1 — a held original is *not* disturbed by it.
    ///
    /// Evaluated on the executor rather than in the handler, which is why it is an [`ExecError`]
    /// and not something the handler decides before submitting.
    BatchConflict { batch_id: String },
    /// `count` of this batch's rows name an external id the live map **already** holds → HTTP 409,
    /// no effect (contracts §3.1's duplicate row).
    ///
    /// **A backstop, not the primary check.** `/control/ingest` already rejects duplicates in the
    /// handler, with a detail naming them. But the live map is written at *apply* time, and Task 3a
    /// moved apply behind a queue — so between a handler's check and the executor's insert there is
    /// now a whole drain, and a client retry under a **fresh** `batch_id` can pass the handler check
    /// twice. Without this the second insert silently overwrites the first, and the first item
    /// stays visible, byte-identical to a suppressed one, and reachable by **no external id at
    /// all** — so no deny can ever name it. Re-checked on the one thread that also performs the
    /// insert, so check and apply cannot be separated (Task 3a security review, C1).
    ///
    /// Carries a **count, never the ids**: this reaches a response body, and an external id is
    /// caller-supplied data `tessera-server`'s `error.rs` keeps out of one. The handler's own check
    /// is the one that names them, to the caller who supplied them.
    DuplicateExternalId { count: usize },
}

impl std::fmt::Display for ExecError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            ExecError::Wal(e) => write!(f, "write-ahead log failure: {e}"),
            ExecError::Alloc(e) => write!(f, "entity-id assignment refused: {e}"),
            ExecError::BatchConflict { batch_id } => write!(
                f,
                "batch id '{batch_id}' was already submitted with a different body"
            ),
            ExecError::DuplicateExternalId { count } => write!(
                f,
                "{count} row(s) name an external id this deployment already knows; the batch had \
                 no effect"
            ),
        }
    }
}

impl std::error::Error for ExecError {}

impl From<WalError> for ExecError {
    fn from(e: WalError) -> Self {
        ExecError::Wal(e)
    }
}

impl From<AllocError> for ExecError {
    fn from(e: AllocError) -> Self {
        ExecError::Alloc(e)
    }
}

/// The executor's answer to one submitted [`Command`].
///
/// **A receipt is a promise that the effect is in force**, not that the command was queued: the
/// executor resolves it strictly *after* the generation carrying the effect has been swapped in
/// (`append → fsync → apply → swap → ack`). The fail-open this ordering exists to prevent is an
/// ack that precedes the swap, letting a client observe a 200 for a suppression that is not yet
/// in force.
///
/// A struct rather than a bare `Result` because later stages give it company — a window sequence
/// number, the group-commit counters Task 10 emits — and widening a struct is additive where
/// changing a type alias is not.
#[derive(Debug)]
pub struct Receipt {
    /// The ack payload, or why there is none. See [`ExecError::Wal`] before assuming an error
    /// here means the command had no effect.
    pub outcome: Result<Ack, ExecError>,
}

impl Receipt {
    /// A receipt for a command that completed.
    pub fn ok(ack: Ack) -> Self {
        Receipt { outcome: Ok(ack) }
    }

    /// A receipt for a command that failed. The caller maps the variant to a status code; see
    /// [`ExecError`] for the mapping and for which variants still applied something.
    pub fn failed(error: impl Into<ExecError>) -> Self {
        Receipt {
            outcome: Err(error.into()),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn row() -> UnallocatedRow {
        UnallocatedRow {
            external_id: Some(b"ext-1".to_vec()),
            descriptors: vec![b"dept:eng".to_vec(), b"region:emea".to_vec()],
            x: 1.5,
            y: -2.5,
            scalars: vec![WalScalar::U64(7), WalScalar::Utf8("s".into())],
            terms: vec![TermId::new(9), TermId::new(2)],
        }
    }

    /// The conversion Task 3a and Task 7a both perform, end to end: rows in, `PendingItem`s to the
    /// allocator, IDs back by position, `WalRow`s out. Every `WalRow` field must come from the
    /// `UnallocatedRow` or from the allocator — if a field could only be filled in with a default,
    /// the type does not line up with `WalRow` and the conversion is not mechanical.
    #[test]
    fn an_unallocated_row_carries_everything_a_wal_row_needs_except_the_id() {
        let original = row();

        let pending = original.to_pending();
        assert_eq!(pending.external_id, original.external_id);
        assert_eq!(pending.terms, original.terms);
        assert!(
            pending.entity_id.is_none(),
            "the whole point: no id until the executor assigns one"
        );

        let wal_row = original.clone().into_wal_row(EntityId::new(41));
        assert_eq!(wal_row.entity_id, EntityId::new(41));
        assert_eq!(wal_row.external_id, original.external_id);
        assert_eq!(wal_row.descriptors, original.descriptors);
        assert_eq!(wal_row.x, original.x);
        assert_eq!(wal_row.y, original.y);
        assert_eq!(wal_row.scalars, original.scalars);
    }

    /// The lane asymmetry, asserted on the vocabulary itself: every `/control/changes` command
    /// takes the never-shed lane regardless of op — including `Unsuppress` and `Predicate`, the
    /// two a reader is most likely to assume are ordinary work — and ingest never does.
    #[test]
    fn every_change_rides_the_never_shed_lane_and_no_ingest_does() {
        for op in [
            ChangeOp::Delete,
            ChangeOp::Suppress,
            ChangeOp::Unsuppress,
            ChangeOp::Predicate,
        ] {
            let cmd = Command::Change {
                external_id: b"ext-1".to_vec(),
                entity: EntityId::new(1),
                op,
                descriptors: None,
            };
            assert!(cmd.is_never_shed(), "{op:?} must not be sheddable for load");
        }
        let ingest = Command::Ingest {
            rows: vec![row()],
            batch_id: "b".into(),
            body_hash: [0u8; 32],
        };
        assert!(!ingest.is_never_shed());
    }
}
