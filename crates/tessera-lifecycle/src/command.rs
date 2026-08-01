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
    /// The allocator's view of this row: `(external_id, terms)`, with no ID yet — **moved out of
    /// the row, not copied**.
    ///
    /// # Why it moves (Task 7a)
    ///
    /// [`crate::assign_sorted`] needs a `PendingItem` to *own* its `external_id` (the sort's
    /// tie-break) and its `terms` (the signature), and the previous shape of this method built one
    /// by cloning both — two heap allocations per row, on the path that must sustain 10⁹ writes,
    /// paid solely to leave the row intact. It does not need to be intact: the ids come back by
    /// position and [`UnallocatedRow::into_wal_row_with`] puts both halves back.
    ///
    /// **The row is hollow between the two calls** — `external_id: None`, `terms` empty — and
    /// nothing may observe it in that state. The interval is one function's gather-to-frame in
    /// `crate::window::CommitWindow::allocate`; the window's conflict check runs at admission,
    /// before it, and the allocation error path drops the entries rather than returning them.
    pub fn take_pending(&mut self) -> PendingItem {
        PendingItem {
            external_id: self.external_id.take(),
            terms: std::mem::take(&mut self.terms),
            entity_id: None,
        }
    }

    /// The WAL's view of this row, reunited with the `PendingItem` [`UnallocatedRow::take_pending`]
    /// took out of it — the exact inverse of that call, and the assigned id.
    ///
    /// Consuming, so the row's heap (`descriptors`, `scalars`) moves into the record rather than
    /// being cloned into it, and `external_id` moves **back** from the `PendingItem`.
    ///
    /// Returns the resolved `terms` alongside, because [`WalRow`] has no `terms` field — the WAL
    /// stores raw descriptors — and the buffer apply needs them. They are returned rather than
    /// dropped so the move is visible: the alternative is a `clone` at the call site, which is the
    /// regression this pair exists to prevent (Task 3a measured its twin at +14% on the 10 000-row
    /// arm).
    ///
    /// Deliberately paired with [`UnallocatedRow::take_pending`] in one place: these two are the
    /// whole of the "mechanical conversion" this type exists to make mechanical, and a field
    /// added to [`WalRow`] without a matching field here is a compile error at exactly this
    /// method rather than a silently-dropped column somewhere downstream.
    pub fn into_wal_row_with(self, pending: PendingItem) -> (WalRow, Vec<TermId>) {
        let entity_id = pending
            .entity_id
            .expect("assign_sorted assigns every item it is given");
        (
            WalRow {
                external_id: pending.external_id,
                entity_id,
                descriptors: self.descriptors,
                x: self.x,
                y: self.y,
                scalars: self.scalars,
            },
            pending.terms,
        )
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
    ///
    /// **A NEW VARIANT DEFAULTS TO THE BOUNDED, SHEDDABLE LANE.** This is a `matches!` over one
    /// variant, so stage 2.2's `Flush` and stage 2.3's `Compact` are sheddable the moment they are
    /// added and nothing warns about it. That default is right for those two — a flush that cannot
    /// be admitted is backpressure working — but it is the wrong default for anything a caller is
    /// owed an unrefusable answer to, and adding a variant without visiting this line is how such a
    /// thing ships. There is no `submit_deny` to reach for instead: the lane follows the command,
    /// and this function is the whole of the rule.
    pub fn is_never_shed(&self) -> bool {
        matches!(self, Command::Change { .. })
    }
}

/// Why a command did not come back with a receipt. Distinct from [`ExecError`], which is why an
/// accepted command failed *while executing*.
///
/// **The variants split on one question, and it is not "is the executor alive".** It is **"could
/// this command have taken effect?"** — because that is the only thing an HTTP status can honestly
/// report, and because a deny that took effect must never be reported as a no-op. The split is
/// structural rather than argued: `Sender::send`/`try_send` hand the value **back** inside their
/// error, so a failed send is a proof of non-enqueue; everything after a successful send is
/// unknown, up to and including fully applied and swapped in.
///
/// *(Corrected at the Task 3b design gate, where four independent reviewers found the same defect:
/// `ExecutorDead` was one variant covering both, its doc asserted "nothing was attempted" as fact,
/// and the Task 3b status table was about to map the whole variant to 503 `not-ready` — a status
/// whose meaning is "this node did not take your write". For a suppression already in force that is
/// the fail-open the deny lane exists to prevent.)*
///
/// Every variant must be surfaced. A handle that swallows a dead executor while still returning
/// 202s is the worst available outcome — the caller believes its write is in flight and it is
/// not, which for a suppression means an item stays visible with an acknowledgement in hand.
///
/// **Deliberately not `#[non_exhaustive]`, and that absence is load-bearing.** `tessera-server`'s
/// `map_accept_error` names every variant of this enum and of [`ExecError`] with no `_` arm, so a
/// new variant is an `E0004` there rather than a silent 500 — but only because neither type permits
/// a cross-crate wildcard. Adding `#[non_exhaustive]` later reads as ordinary API hygiene for a
/// `pub` enum and would turn that compile error into a permitted catch-all, which is how a new
/// outcome acquires a status nobody chose for it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SubmitError {
    /// The bounded work queue is full → HTTP 429 with `Retry-After: retry_after_s` (contracts
    /// §3.1's 429 row). Nothing was enqueued: `try_send` returned the job.
    ///
    /// **Reachable only from the ingest lane.** A command for which
    /// [`Command::is_never_shed`] holds is submitted to the unbounded queue and can never
    /// produce this variant; Task 6's `changes_never_429s` is the test that asserts it.
    QueueFull { retry_after_s: u64 },
    /// The executor could not be **handed** the command — it was never started, or the queue it
    /// belongs to is disconnected. → HTTP 503 `not-ready`, and the planes report not-ready.
    ///
    /// **Nothing was attempted, and that is a fact about the code rather than an expectation of
    /// it.** The invariant every producer must satisfy is *non-enqueue is proven* — not the
    /// syntactic test an earlier revision gave ("every producer is a `send` that failed"), which
    /// already failed for one of the three: `WritePath::handle` produces this from
    /// `self.handle.as_ref().ok_or(..)`, where there is no channel to send on because the executor
    /// was never started. That conclusion is *stronger* than a failed send, so the mapping is right;
    /// stating the weaker syntactic rule invites a future producer to be checked against a test its
    /// own siblings fail. The two send-shaped producers are instances of the invariant:
    /// `Sender::send`/`try_send` hand the job **back** inside their error, so a failed send is a
    /// proof of non-enqueue.
    ///
    /// That invariant is what makes 503 — "this node did not take your write" — honest here and
    /// dishonest for [`SubmitError::ReceiptLost`]. A new producer belongs here only if it can prove
    /// the same thing; if it cannot, it is a `ReceiptLost`.
    ///
    /// Reachable from **both** lanes: a deny is never refused for *load*, which is not the same
    /// as never refused. There is no honest 200 to give when there is nothing left to apply it.
    ExecutorDead,
    /// The command **was** enqueued and no receipt came back: the executor died holding it.
    /// → HTTP 500 fail-closed, **never 503**.
    ///
    /// **Its disposition is unknown, and "unknown" includes "fully applied".** The executor's
    /// sequence is `append → fsync → apply → swap → ack`, so a death anywhere after the swap
    /// leaves a durable, in-force effect with no receipt — for a `Suppress`, an item that is
    /// already hidden. Reporting that as 503 would tell an operator nothing happened and invite
    /// them to act as though the item were still visible.
    ///
    /// Two producers, and both are genuinely post-enqueue: a disconnected doorbell (rung *after*
    /// the job is in the queue, and the executor's shutdown pass drains and **executes** the deny
    /// queue before it observes the disconnect), and a dropped responder.
    ///
    /// **Only the dropped responder is reachable on the panic path today, and the reason is a field
    /// order.** `LifecycleQueues` declares `work, deny, bell`, so during the executor's unwind the
    /// two job receivers disconnect *before* the bell does — a submitter racing that window is
    /// refused at its `send` with [`SubmitError::ExecutorDead`] and returns before the bell is rung.
    /// The doorbell producer is therefore a correct answer to a state nothing currently reaches, kept
    /// because reordering those fields (or giving the bell an independent lifetime) makes it live,
    /// and the answer it gives is the right one either way. `tessera-engine`'s
    /// `an_executor_panic_is_reported_dead` pins the reachable one at its producer.
    ///
    /// Stage 2.1 makes this narrow — nothing fallible sits between the swap and the ack — but
    /// Task 7a widens it structurally: a commit window performs one swap and then acks N waiters
    /// in a loop, so a death partway through hands every remaining waiter this error with its
    /// effect already in force. The variant exists now so that table is right when it arrives.
    ReceiptLost,
}

impl SubmitError {
    /// Whether the command may have taken effect. `false` only where non-enqueue is proven.
    ///
    /// The one question a batch-level answer needs (`tessera-server`'s `map_change_batch_error`),
    /// asked here rather than by matching on variants at the call site — a caller re-deriving it
    /// is one forgotten variant away from reporting an applied suppression as a no-op.
    pub fn may_have_taken_effect(&self) -> bool {
        match self {
            SubmitError::QueueFull { .. } | SubmitError::ExecutorDead => false,
            SubmitError::ReceiptLost => true,
        }
    }
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
            SubmitError::ReceiptLost => write!(
                f,
                "the write executor died holding this command; it may have been applied in full"
            ),
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
    ///
    /// **At batch scope, one of these does not stop the batch.** A `/control/changes` request is a
    /// list, and `tessera-server`'s `run_changes` submits **every** validated item even after one
    /// of them fails this way, then reports the first failure. That matters because
    /// [`crate::wal::WalError::Poisoned`] is a sustained posture, not a transient: a batch against
    /// a poisoned node would otherwise apply exactly its first item on every retry until the WAL is
    /// reopened, leaving the rest of the denies unapplied under a 500 that says durability is owed.
    /// So the 500 means "at least one item is in force but not durable, and every deny in the
    /// request was attempted", never "the batch was refused".
    Wal(WalError),
    /// Entity-ID assignment refused (I9's `u32` ceiling) → HTTP 500, fail closed. The batch has
    /// no effect: `Allocator::allocate` leaves the high-water mark unchanged on this path.
    Alloc(AllocError),
    /// This `batch_id` was already accepted, or is held in an open window, with **different**
    /// body bytes → HTTP 409 (contracts §3.4). The retry has no effect, and **a held original is
    /// not disturbed by it** — Task 8 implemented that as the owner-confirmable default; the
    /// argument, and what changes if the owner rules the other way, are at the one site that
    /// decides it (`tessera-engine`'s `Executor::admit_ingest`, the `Held` arm).
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
    ///
    /// Task 7a made the pair a **round trip** rather than two independent reads, so this asserts the
    /// round trip: every field must arrive on the far side, including the two that now travel
    /// *through* the `PendingItem` (`external_id`, `terms`) rather than staying in the row. The
    /// hollow interval between the two calls is asserted too — it is the price of not cloning, and a
    /// reader who does not know about it would be surprised by it exactly once.
    #[test]
    fn an_unallocated_row_carries_everything_a_wal_row_needs_except_the_id() {
        let original = row();
        let mut row = original.clone();

        let mut pending = row.take_pending();
        assert_eq!(pending.external_id, original.external_id);
        assert_eq!(pending.terms, original.terms);
        assert!(
            pending.entity_id.is_none(),
            "the whole point: no id until the executor assigns one"
        );
        assert!(
            row.external_id.is_none() && row.terms.is_empty(),
            "the two fields MOVED: the row is hollow until `into_wal_row_with` reunites them"
        );

        // What `assign_sorted` does, at one item's scale.
        pending.entity_id = Some(EntityId::new(41));

        let (wal_row, terms) = row.into_wal_row_with(pending);
        assert_eq!(wal_row.entity_id, EntityId::new(41));
        assert_eq!(wal_row.external_id, original.external_id);
        assert_eq!(wal_row.descriptors, original.descriptors);
        assert_eq!(wal_row.x, original.x);
        assert_eq!(wal_row.y, original.y);
        assert_eq!(wal_row.scalars, original.scalars);
        assert_eq!(
            terms, original.terms,
            "and the resolved terms come back too"
        );
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
