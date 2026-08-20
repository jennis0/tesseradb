//! The lifecycle command vocabulary: what a handler submits to the write executor, and what it
//! gets back.
//!
//! ## Where the executor lives, and why the vocabulary lives here
//!
//! The executor thread itself is **not** in this crate — it is in `tessera-engine`'s `write.rs`.
//! Its loop is append → fsync → **apply → swap** → ack, and apply/swap clone the
//! `IngestBuffer`/`Overlay` out of a `Generation`, which holds a `tessera_store::Bundle`;
//! `tessera-engine` depends on this crate, so a thread here importing `Generation` is a cycle
//! cargo refuses, and this crate deliberately has no `tessera-store` dependency. Everything in
//! this module is entity-space and store-free, which is exactly the half that *can* live here.
//!
//! ## Why the commands carry unallocated rows
//!
//! [`crate::wal::WalRow`]'s `entity_id` is mandatory, so a command carrying `WalRow`s would force
//! the **handler** to allocate before submitting — and that makes window-scoped allocation
//! impossible, because by the time several submissions reach one commit window their IDs are
//! already fixed. The window's whole purpose is to sort and assign them together
//! (lifecycle §5.1, and [`crate::window`] for the mechanism). So allocation happens on the
//! executor, and this type is what a handler submits: everything a `WalRow` needs *except* the ID,
//! plus the resolved term set that is the item's sort signature.

use tessera_types::{EntityId, TermId};

use crate::alloc::{AllocError, PendingItem};
use crate::wal::{ChangeOp, WalError, WalRow, WalScalar};

/// One ingest row awaiting entity-ID assignment on the executor.
///
/// Field-for-field [`WalRow`] minus `entity_id`, plus `terms`. Both halves matter:
///
/// - `descriptors` are the **raw descriptor bytes**, carried because that is what the WAL record
///   stores — term IDs are bundle-relative ordinals, and a term coined between builds has no
///   durable ID at all, so a `WalRow` cannot be framed from `terms` alone. Without this field the
///   executor cannot build the record it is supposed to append.
/// - `terms` are the **already-resolved** `TermId`s, and they are here because signature-sorted
///   assignment (I9, design §11.1) needs each item's resolved term set to compute its sort key
///   *before* any ID exists. Resolution therefore happens in the handler, ahead of the durability
///   boundary — a structural exception argued at `WritePath::resolve_terms` in `tessera-engine`,
///   not an oversight to be tidied up by moving it onto the executor.
///
/// `external_id` is optional (contracts §3.4 r6) and `None` must never collide with `None`: an
/// item with no external ID is addressable only by its `tessera_id`, is established in no live
/// map, and is not a duplicate of any other such item.
///
/// `view` is resolved by the handler against the bundle's declared views — never defaulted here
/// — for the reason given at [`WalRow`]'s own field.
#[derive(Debug, Clone, PartialEq)]
pub struct UnallocatedRow {
    pub external_id: Option<Vec<u8>>,
    pub view: String,
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
    /// # Why it moves
    ///
    /// [`crate::assign_sorted`] needs a `PendingItem` to *own* its `external_id` (the sort's
    /// tie-break) and its `terms` (the signature). Building one by cloning both costs two heap
    /// allocations per row, on the path that must sustain 10⁹ writes, and buys only leaving the row
    /// intact — which it does not need to be: the ids come back by position and
    /// [`UnallocatedRow::into_wal_row_with`] puts both halves back.
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
    /// regression this pair exists to prevent (its twin was measured at +14% on the 10 000-row
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
                view: self.view,
                descriptors: self.descriptors,
                x: self.x,
                y: self.y,
                scalars: self.scalars,
            },
            pending.terms,
        )
    }
}

/// **An artifact one ingest batch's rows join**, named by the key the caller's column carried and
/// pointing back at the rows that carried it (`artifacts-from-points.md` §6.2).
///
/// **Rows, not entities, because the entities do not exist yet.** A batch's ids are assigned when
/// its commit window closes, so a membership column read at the boundary can only say *which rows
/// of this batch* named the key; the executor turns those positions into entities after the
/// assignment and before the append, which is what puts the join in the same commit as the rows.
///
/// **The key travels as a key**, on [`Command::PublishArtifacts`]'s rule: `ordinal_of_key` reads
/// state only the executor may write. It is resolved once, at admission, and the ordinal is carried
/// from there — recorded rather than re-derived, so what the log holds is what was decided.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BatchMembership {
    pub layer: String,
    pub level: u32,
    pub key: String,
    /// Indices into this batch's `rows`, ascending and without repeats.
    pub rows: Vec<u32>,
}

/// **A parent edge one batch's list column declared**, as the caller's own keys spell it.
///
/// Carried beside the memberships rather than folded into them because it is a different claim
/// about the same data: an entry names a membership, and *consecutive* entries name an edge
/// ([`tessera_types::layer::parent_edges`]). The wire route cannot create an edge — a growth adds
/// members and never lineage — so what the executor does with one is check it against the edge the
/// publication already stored, and refuse where the two disagree.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BatchEdge {
    pub layer: String,
    /// The child's level; the parent sits at this level for a lineage and one coarser for a tiered
    /// containment, which is the resolution `LayerRegistry` already performs at publication.
    pub level: u32,
    pub child: String,
    pub parent: String,
}

/// What one ingest batch's membership column said (`artifacts-from-points.md` §6.2): which
/// artifacts its rows join, and which parent edges its adjacency declared.
///
/// Default-empty, and that is every batch that names no layer — the overwhelming majority, and the
/// shape of the path before this existed.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct BatchArtifacts {
    pub memberships: Vec<BatchMembership>,
    pub edges: Vec<BatchEdge>,
}

impl BatchArtifacts {
    pub fn is_empty(&self) -> bool {
        self.memberships.is_empty() && self.edges.is_empty()
    }
}

/// One unit of work for the write executor.
///
/// Ingest and change are the whole vocabulary. A flush and a compaction fold are specified as
/// further commands and would arrive **additively** — new variants, not a changed shape for these
/// two.
/// **⊘ Specified, not implemented.** Neither exists, so nothing today submits anything but these.
#[derive(Debug, Clone, PartialEq)]
pub enum Command {
    /// An accepted `/control/ingest` batch, rows not yet allocated (see [`UnallocatedRow`]).
    ///
    /// `batch_id`/`body_hash` are the idempotency key material and travel with the command
    /// because the batch-state lookup is evaluated **on the executor, not in the handler**:
    /// between a handler check and the enqueue an open window can close, and a retry that saw
    /// `Unknown` and then enqueued into a fresh window has double-allocated.
    Ingest {
        rows: Vec<UnallocatedRow>,
        batch_id: String,
        body_hash: [u8; 32],
        /// The artifacts this batch's rows named in a column named for a layer — empty for a batch
        /// that named none (§6.2). Resolved and grown when the window closes, in the same commit as
        /// the rows, so there is no state in which a point is ingested and its membership is not.
        artifacts: BatchArtifacts,
    },
    /// One accepted `/control/changes` entry.
    ///
    /// **Addressed by entity, whatever the caller supplied.** The handler resolves an
    /// `external_id` through the live map and the bundle sidecar, or inverts a `tessera_id`, and
    /// carries the result — so the executor re-resolves nothing inside its critical section, and
    /// the record it appends names the entity rather than an identifier whose meaning depends on
    /// a key (`WalRecord::ChangeByEntity`). An item ingested without an external id is addressable
    /// only this way, which is the hole addressing by entity closes.
    Change { entity: EntityId, op: ChangeOp },
    /// Register an annotation layer.
    ///
    /// **The declaration travels unvalidated and unallocated**, in the same shape and for the same
    /// reason as [`Command::Ingest`]'s rows: the checks that decide a name is free, and the
    /// allocations that follow them, both read state only the executor may write. A handler that
    /// validated first could be overtaken by a registration of the same name between its check and
    /// the enqueue, and would then have acked two layers onto one name.
    /// **Boxed** so one large variant does not set the size of every command in the queue: a
    /// declaration is the biggest thing that travels here by a wide margin, and `Ingest` and
    /// `Change` are the two the executor moves at rate.
    RegisterLayer {
        declaration: Box<tessera_types::layer::LayerDeclaration>,
    },
    /// Drop an annotation layer, tombstoning its name for ever.
    DropLayer { name: String },
    /// Publish a batch of artifacts into one level of one layer.
    ///
    /// **Members are entities already.** The handler inverts the caller's `tessera_id`s once, at
    /// the boundary, on the same rule [`Command::Change`] follows: a blinded identifier's meaning
    /// depends on a key, so carrying one into the executor — and from there into the log — would
    /// let a rotation silently redirect a membership.
    ///
    /// Ordinals are **not** carried: they are claimed on the executor from the level's cursor, for
    /// the reason [`Command::RegisterLayer`] leaves its name check there. Two batches admitted
    /// concurrently would otherwise be handed the same ordinals and the second would overwrite the
    /// first's artifacts in place.
    PublishArtifacts {
        layer: String,
        level: u32,
        artifacts: Vec<crate::membership::IncomingArtifact>,
    },
    /// Add entities to the memberships of artifacts that **already exist**, each named by the key
    /// it was published under.
    ///
    /// **Members are entities already**, on [`Command::PublishArtifacts`]'s rule, and the keys are
    /// **not** resolved here: `ordinal_of_key` reads state only the executor may write, so a
    /// handler that resolved first could be overtaken by a fold retiring the artifact between its
    /// lookup and the enqueue, and would have grown an ordinal a later publication now holds.
    ///
    /// The whole batch or none of it: a key that names no artifact refuses the command rather than
    /// growing the rest, so a caller is never left unable to say which of their joins happened.
    ///
    /// **It rides the bounded, sheddable lane**, like the publication it grows — a join refused for
    /// load is backpressure and the caller retries, where a deny refused for load is an item left
    /// visible. See [`Command::is_never_shed`].
    GrowMemberships {
        layer: String,
        level: u32,
        joins: Vec<crate::membership::IncomingGrowth>,
    },
}

impl Command {
    /// Whether this command rides the **never-refused** lane (lifecycle §1.3's deny priority
    /// lane): the unbounded queue the executor drains to empty before it touches work.
    ///
    /// The lane is chosen by **endpoint, not by op**. Every `/control/changes` entry takes it,
    /// including [`ChangeOp::Unsuppress`], because the property being
    /// preserved is contracts §3.1's: `/control/changes` cannot answer 429. Batching a security
    /// operation for latency is acceptable; refusing one for load is not — and an `Unsuppress`
    /// shed for load leaves an item hidden that a caller was told to expect back, which is a
    /// different failure but not a better one.
    ///
    /// Two consequences, chosen rather than discovered: a sustained deny flood starves ingest
    /// completely, and this queue is unbounded in memory.
    ///
    /// **A NEW VARIANT DEFAULTS TO THE BOUNDED, SHEDDABLE LANE.** This is a `matches!` over one
    /// variant, so a `Flush` or a `Compact` is sheddable the moment it is
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
/// *The single-variant form is the trap to avoid: one `ExecutorDead` covering both cases maps to
/// 503 `not-ready`, whose meaning is "this node did not take your write". For a suppression already
/// in force that is the fail-open the deny lane exists to prevent.*
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
    /// produce this variant; `changes_never_429s` is the test that asserts it.
    QueueFull { retry_after_s: u64 },
    /// The executor could not be **handed** the command — it was never started, or the queue it
    /// belongs to is disconnected. → HTTP 503 `not-ready`, and the planes report not-ready.
    ///
    /// **Nothing was attempted, and that is a fact about the code rather than an expectation of
    /// it.** The invariant every producer must satisfy is *non-enqueue is proven* — deliberately
    /// not the narrower syntactic test "every producer is a `send` that failed", which one of the
    /// three producers does not meet: `WritePath::handle` produces this from
    /// `self.handle.as_ref().ok_or(..)`, where there is no channel to send on because the executor
    /// was never started. That conclusion is *stronger* than a failed send, so the mapping is right;
    /// stating the syntactic rule instead would invite a future producer to be checked against a
    /// test its own siblings fail. The two send-shaped producers are instances of the invariant:
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
    /// Nothing fallible sits between the swap and the ack, but the group-commit shape widens this
    /// structurally: a commit window performs one swap and then acks N waiters in a loop, so a
    /// death partway through hands every remaining waiter this error with its effect already in
    /// force.
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
    /// A disposition change applied. Nothing to return: the caller named the item.
    Changed,
    /// A layer was registered. The entity is returned so the handler can hand back its
    /// `tessera_id` — the only address by which a caller can later suppress the layer, since an
    /// entity id never crosses the boundary (**I10**).
    LayerRegistered { entity: EntityId },
    /// A layer was dropped and its name tombstoned. Nothing to return: the caller named it.
    LayerDropped,
    /// Artifacts were published, in the caller's submitted order.
    ///
    /// **Entities, which the handler turns into `tessera_id`s — never the ordinals.** An ordinal is
    /// a position in a dense level, so a caller holding two of them learns how many artifacts sit
    /// between; across two principals it is a corpus-wide count over objects one of them may not
    /// see, which is C8's row. The `tessera_id` is the only artifact address that crosses the wire.
    ArtifactsPublished { entities: Vec<EntityId> },
    /// Memberships grew. **Nothing to return: the caller named the artifacts**, by the keys they
    /// published them under — the same reason [`Ack::Changed`] carries nothing. No identity was
    /// minted, so there is no new `tessera_id` to hand back, and the ordinals the growth resolved
    /// to are exactly what never crosses the wire (C8).
    MembershipsGrown,
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
    /// not disturbed by it**; the argument for that reading, and what would change under the
    /// opposite one, are at the one site that decides it (`tessera-engine`'s
    /// `Executor::admit_ingest`, the `Held` arm).
    ///
    /// Evaluated on the executor rather than in the handler, which is why it is an [`ExecError`]
    /// and not something the handler decides before submitting.
    BatchConflict { batch_id: String },
    /// A category key could not acquire a code → HTTP 422 naming the vocabulary and its width. The
    /// batch has no effect: nothing was appended and nothing applied.
    ///
    /// **Code-space exhaustion is the case this exists for**, and per-point-attributes §3.6 makes
    /// it a `422` at ingest rather than a 500 — the request names more distinct values than the
    /// declared width can hold, which is the caller's own data measured against the deployment's
    /// published schema, exactly the shape `AcceptError::OutsideExtent` already answers that way.
    /// Widening or wrapping instead would recolour every row already carrying a code, so neither
    /// is done and the refusal is the whole answer.
    ///
    /// **A rendered string, not the error itself.** Minting lives in `tessera-store`, and this
    /// crate deliberately carries no dependency on it (see this module's header), so the detail is
    /// rendered on the executor and travels as text. It names a vocabulary and a width — the
    /// deployment's own schema, never a filesystem path — so unlike most executor failures it may
    /// reach the caller, who cannot otherwise act on it.
    VocabularyRefused { detail: String },
    /// `count` of this batch's rows name an external id the live map **already** holds → HTTP 409,
    /// no effect (contracts §3.1's duplicate row).
    ///
    /// **A backstop, not the primary check.** `/control/ingest` already rejects duplicates in the
    /// handler, with a detail naming them. But the live map is written at *apply* time, and apply
    /// happens behind a queue — so between a handler's check and the executor's insert there is
    /// a whole drain, and a client retry under a **fresh** `batch_id` can pass the handler check
    /// twice. Without this the second insert silently overwrites the first, and the first item
    /// stays visible, byte-identical to a suppressed one, and reachable by **no external id at
    /// all** — so no deny can ever name it. Re-checked on the one thread that also performs the
    /// insert, so check and apply cannot be separated.
    ///
    /// Carries a **count, never the ids**: this reaches a response body, and an external id is
    /// caller-supplied data `tessera-server`'s `error.rs` keeps out of one. The handler's own check
    /// is the one that names them, to the caller who supplied them.
    DuplicateExternalId { count: usize },
    /// A layer registration or drop was refused → HTTP 422, no effect. Every check runs before the
    /// first allocation and before the WAL append, so a refusal leaves no ids spent, no record in
    /// the log and no half-registered layer.
    ///
    /// **A rendered string, and it may reach the caller.** What it names is the caller's own
    /// declaration measured against the deployment's published rules — a name already taken, a
    /// name tombstoned, a tree declaring levels — which is exactly the class of detail a caller can
    /// act on and cannot otherwise obtain. It names no entity, no path and no other layer's terms.
    LayerRefused { detail: String },
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
            ExecError::VocabularyRefused { detail } => write!(f, "{detail}"),
            ExecError::LayerRefused { detail } => write!(f, "{detail}"),
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
/// A struct rather than a bare `Result` because it is expected to acquire company — a window
/// sequence number, group-commit counters — and widening a struct is additive where changing a type
/// alias is not.
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
            view: "default".to_string(),
            descriptors: vec![b"dept:eng".to_vec(), b"region:emea".to_vec()],
            x: 1.5,
            y: -2.5,
            scalars: vec![WalScalar::U64(7), WalScalar::Utf8("s".into())],
            terms: vec![TermId::new(9), TermId::new(2)],
        }
    }

    /// The conversion the executor performs, end to end: rows in, `PendingItem`s to the
    /// allocator, IDs back by position, `WalRow`s out. Every `WalRow` field must come from the
    /// `UnallocatedRow` or from the allocator — if a field could only be filled in with a default,
    /// the type does not line up with `WalRow` and the conversion is not mechanical.
    ///
    /// The pair is a **round trip** rather than two independent reads, so this asserts the
    /// round trip: every field must arrive on the far side, including the two that travel
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
    /// takes the never-shed lane regardless of op — including `Unsuppress`, the one a reader is
    /// most likely to assume is ordinary work — and ingest never does.
    #[test]
    fn every_change_rides_the_never_shed_lane_and_no_ingest_does() {
        for op in [ChangeOp::Delete, ChangeOp::Suppress, ChangeOp::Unsuppress] {
            let cmd = Command::Change {
                entity: EntityId::new(1),
                op,
            };
            assert!(cmd.is_never_shed(), "{op:?} must not be sheddable for load");
        }
        let ingest = Command::Ingest {
            rows: vec![row()],
            batch_id: "b".into(),
            body_hash: [0u8; 32],
            artifacts: Default::default(),
        };
        assert!(!ingest.is_never_shed());
    }
}
