//! What a write command carries, and why an accepted one can still fail.
//!
//! ## Where the commands themselves live
//!
//! The executor thread is **not** in this crate — it is in `mosaica-engine`'s `write` module, and
//! so is the command enum it consumes. Its loop is append → fsync → **apply → swap** → ack, and
//! apply/swap clone the `IngestBuffer`/`Overlay` out of a `Generation`, which holds a
//! `mosaica_store::Bundle`; `mosaica-engine` depends on this crate, so a thread here importing
//! `Generation` is a cycle cargo refuses, and this crate deliberately has no `mosaica-store`
//! dependency. What is here is the half that is entity-space and store-free: the rows and requests
//! a command carries, and the two errors `mosaica-server` maps to HTTP statuses.
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

use mosaica_types::{EntityId, TermId};

use crate::alloc::{AllocError, PendingItem};
use crate::wal::{WalError, WalRow, WalScalar};

/// One row of an ingest batch as the caller sent it: the values that identify the item it names
/// and the values it carries, each of which may be left out.
///
/// A left-out value keeps what the named item stores, and a new item has no value there. A value
/// sent as null clears it. `omitted` lists the columns the row left out, as positions in the
/// declared scalars followed by the batch view's group-scoped families; each such position holds
/// its absence in `scalars` or `scoped`.
#[derive(Debug, Clone, PartialEq)]
pub struct IngestRow {
    pub mosaica_id: Option<mosaica_types::MosaicaId>,
    /// The row's access labels. `None` where the row left its label out, which keeps a named
    /// item's label and gives a new item the view's default. `Some` of an empty list is no label.
    pub labels: Option<Vec<Vec<u8>>>,
    /// Frame coordinates in the batch's view, or `None` where the row carries none.
    pub position: Option<(f64, f64)>,
    pub scalars: Vec<WalScalar>,
    pub scoped: Vec<WalScalar>,
    pub omitted: Vec<usize>,
}

/// One row the handler has resolved to a write, awaiting entity-ID assignment on the executor: a
/// row that creates an item, or one that adds an existing item to its view.
///
/// Field-for-field [`WalRow`] minus `entity_id`, plus `terms`. `descriptors` are the raw bytes the
/// WAL record stores, since a term coined between builds has no durable id. `terms` are the
/// resolved ids signature-sorted assignment needs before any entity id exists.
///
/// `x`/`y` are frame coordinates: a projected view's transform has already run at the wire
/// boundary, so nothing below this type projects anything.
#[derive(Debug, Clone, PartialEq)]
pub struct UnallocatedRow {
    pub view: String,
    /// The existing item this row adds to `view`, or `None` for a row that creates one.
    pub join: Option<mosaica_types::EntityId>,
    pub descriptors: Vec<Vec<u8>>,
    pub x: f64,
    pub y: f64,
    pub scalars: Vec<WalScalar>,
    /// This row's group-scoped attribute values ([`WalRow::scoped`]), positional against the
    /// owning group's `scoped_scalars`, and empty for every view outside a scope.
    pub scoped: Vec<WalScalar>,
    pub terms: Vec<TermId>,
}

impl UnallocatedRow {
    /// The allocator's view of this row: its `terms`, with no ID yet — **moved out of the row, not
    /// copied**.
    ///
    /// # Why it moves
    ///
    /// [`crate::assign_sorted`] needs a `PendingItem` to *own* its `terms` (the signature).
    /// Building one by cloning costs a heap allocation per row, on the path that must sustain 10⁹
    /// writes, and buys only leaving the row intact — which it does not need to be: the ids come
    /// back by position and [`UnallocatedRow::into_wal_row_with`] puts the terms back.
    ///
    /// **The row is hollow between the two calls** — `terms` empty — and nothing may observe it in
    /// that state. The interval is one function's gather-to-frame in
    /// `crate::window::CommitWindow::allocate`; the window's conflict check runs at admission,
    /// before it, and the allocation error path drops the entries rather than returning them.
    pub fn take_pending(&mut self) -> PendingItem {
        PendingItem {
            terms: std::mem::take(&mut self.terms),
            // **A join arrives with its id already decided, and `assign_sorted` leaves it
            // alone.** The entity exists; a second allocation for it would be a second identity
            // for one document, which is the whole of what the join rule prevents.
            entity_id: self.join,
        }
    }

    /// The WAL's view of this row, reunited with the `PendingItem` [`UnallocatedRow::take_pending`]
    /// took out of it — the exact inverse of that call, and the assigned id.
    ///
    /// Consuming, so the row's heap (`descriptors`, `scalars`) moves into the record rather than
    /// being cloned into it.
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
                entity_id,
                view: self.view,
                join: self.join.is_some(),
                descriptors: self.descriptors,
                x: self.x,
                y: self.y,
                scalars: self.scalars,
                scoped: self.scoped,
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
/// **The key travels as a key**, on the publication command's rule: `ordinal_of_key` reads
/// state only the executor may write. It is resolved once, at admission, and the ordinal is carried
/// from there — recorded rather than re-derived, so what the log holds is what was decided.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BatchMembership {
    pub layer: String,
    pub level: u32,
    /// The key of the view the artifact is in, on a group-scoped layer; `None` on an
    /// entity-scoped one.
    pub view: Option<String>,
    pub key: String,
    /// Indices into this batch's `rows`, ascending and without repeats.
    pub rows: Vec<u32>,
}

/// **A parent edge one batch's list column declared**, as the caller's own keys spell it.
///
/// Carried beside the memberships rather than folded into them because it is a different claim
/// about the same data: an entry names a membership, and *consecutive* entries name an edge
/// ([`mosaica_types::layer::parent_edges`]). The executor decides each one against what the layer
/// holds: the same edge is nothing to do, no edge at all is one to record, and a different parent is
/// a refusal.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BatchEdge {
    pub layer: String,
    /// The child's level; the parent sits at this level for a lineage and one coarser for a tiered
    /// containment, which is the resolution `LayerRegistry` already performs at publication.
    pub level: u32,
    /// The view both ends are in, as on [`BatchMembership::view`].
    pub view: Option<String>,
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

/// An item a row edits, as the handler resolved it: the entity it leaves, its number, and the rows
/// its new entity takes, awaiting that entity's id on the executor.
#[derive(Debug, Clone, PartialEq)]
pub struct UnallocatedEdit {
    pub old: EntityId,
    /// The entity the item was first given, which its `mosaica_id` is taken from.
    pub number: EntityId,
    /// The first row carries the item's label and every declared value, and its `terms`; each
    /// other row places the item in one more view and carries that view's position and
    /// group-scoped values alone. Every `join` is `None`: all of them take the new entity.
    pub rows: Vec<UnallocatedRow>,
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
/// **Deliberately not `#[non_exhaustive]`, and that absence is load-bearing.** `mosaica-server`'s
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
    /// `Command::is_never_shed` holds is submitted to the unbounded queue and can never
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
    /// and the answer it gives is the right one either way. `mosaica-engine`'s
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
    /// The one question a batch-level answer needs (`mosaica-server`'s `map_change_batch_error`),
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

/// One join's receipt: what a membership growth did to the artifact one join named.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MembershipGrown {
    /// The artifact's own entity: the address its `mosaica_id` blinds, and the one a later
    /// suppression names.
    pub entity: EntityId,
    /// How many of the joining members were not already in the membership. Zero where the join
    /// named the artifact and added nothing to it, which is accepted rather than refused.
    pub joined: u64,
    /// How many of the fixed parts the join carried were absent and are now held (`ingest.md`
    /// §1.5). A part held identically counts nothing, on `joined`'s rule.
    pub filled: u64,
    /// How many of the leaving members a generating-set page took out of it, on `joined`'s rule
    /// and bounded by the caller's own list. Zero on a membership row: a membership never shrinks
    /// (`ingest.md` §10, R7).
    pub left: u64,
    /// The rank this page emptied, where it emptied one. The content record is removed by the same
    /// delta and does not return when the set refills; the caller supplies it again
    /// (`ingest.md` §1.1). `None` on every other row.
    pub withdrawn: Option<u16>,
}

/// `PUT /control/attributes`' body as the executor resolves it: the `[[attribute]]` block minus
/// its acquisition keys (`configuration.md` §6), with the two spellings a category has.
///
/// **Not the WAL record.** [`crate::wal::AttributeDeclaration`] stores a category at its width,
/// because the width is what a row stores and what the manifest records; a caller declares
/// `type = "category"` and names the vocabulary, as the build's block does, and the width is the
/// vocabulary's. A vocabulary is one code space whichever columns draw on it, so where a column
/// already names it the width is that column's and `width` here must agree or be absent; where
/// none does, `width` is required, because the manifest does not carry a width for a vocabulary
/// no column names (`Vocabularies::seed`). The executor resolves the spelling to the record.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AttributeRequest {
    pub name: String,
    pub title: Option<String>,
    /// The declared type by its `configuration.md` §6 name: a storage type, or `category`.
    pub ty: String,
    pub vocabulary: Option<String>,
    pub analyser: Option<String>,
    pub index: bool,
    pub render: bool,
    pub scope: mosaica_types::layer::LayerScope,
    /// On a column that exists, declaring or removing `unique` is the one change accepted.
    pub unique: bool,
}

/// A vocabulary as `PUT /control/vocabularies/{name}` declares it: the `[[vocabulary]]` block
/// minus its acquisition keys (`source`, `fields`), with the values that fit the request inline
/// (`configuration.md` §1; `ingest.md` §1.3).
///
/// **No code travels here**, at the door or in the request: codes are the server's to assign
/// (per-point-attributes §3.1), so the executor draws each one and records it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct VocabularyRequest {
    pub name: String,
    pub title: Option<String>,
    pub kind: mosaica_types::vocabulary::VocabularyKind,
    pub visibility: mosaica_types::vocabulary::Visibility,
    /// The code space's width by its contracts §2.2 name: `u8`, `u16` or `u32`.
    pub width: String,
    pub values: Vec<DeclaredValue>,
    /// Retired codes, never drawn.
    pub reserved: Vec<u32>,
}

/// One value a declaration or a page supplies: the stable opaque key, and its presentation.
///
/// **The key is not the display name** (per-point-attributes §3.4): `sev_1` is what a row's code
/// stands for and "Critical" is a property of it, so the two are separate fields and a title is
/// amendable in neither direction without the value being resupplied identically.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DeclaredValue {
    pub key: String,
    pub title: Option<String>,
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
    /// list, and `mosaica-server`'s `run_changes` submits **every** validated item even after one
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
    /// opposite one, are at the one site that decides it (`mosaica-engine`'s
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
    /// **A rendered string, not the error itself.** Minting lives in `mosaica-store`, and this
    /// crate deliberately carries no dependency on it (see this module's header), so the detail is
    /// rendered on the executor and travels as text. It names a vocabulary and a width — the
    /// deployment's own schema, never a filesystem path — so unlike most executor failures it may
    /// reach the caller, who cannot otherwise act on it.
    VocabularyRefused { detail: String },
    /// A layer registration or drop was refused → HTTP 422, no effect. Every check runs before the
    /// first allocation and before the WAL append, so a refusal leaves no ids spent, no record in
    /// the log and no half-registered layer.
    ///
    /// **A rendered string, and it may reach the caller.** What it names is the caller's own
    /// declaration measured against the deployment's published rules — a name already taken, a
    /// name tombstoned, a tree declaring levels — which is exactly the class of detail a caller can
    /// act on and cannot otherwise obtain. It names no entity, no path and no other layer's terms.
    LayerRefused { detail: String },
    /// An artifact record supplied a fixed part the artifact already holds, and the two differ
    /// (`ingest.md` §1.1) → **409**. Separate from [`Self::LayerRefused`] on
    /// [`Self::ViewConflict`]'s argument: the caller's remedy differs. The detail names the part
    /// and never the held value.
    PartConflict { detail: String },
    /// A view create or drop the roster refused on its own terms — the key's charset, the
    /// metadata against the group's declaration, a gate this build cannot honour → **422**.
    ViewRefused { detail: String },
    /// A key that is already a view of the group, or one a drop has burnt → **409**. Separate from
    /// [`Self::ViewRefused`] because the caller's remedy differs: a refused record is one to
    /// correct and resubmit, and a taken key is one to replace — a roster record is immutable, so
    /// there is no resubmission that would make it land (`views.md` §3.2).
    ViewConflict { detail: String },
    /// A group or a key this deployment does not carry → **404**, the same answer an unknown view
    /// gets on every other surface.
    ViewUnknown { detail: String },
    /// An attribute declaration measured against the deployment's rules and refused → HTTP
    /// **422**, no effect: a reserved or malformed name, a type outside the declarable set, a
    /// vocabulary or group the deployment does not carry, or a flag combination the schema
    /// refuses (`configuration.md` §6). The text is the caller's own declaration measured against
    /// the published schema and names nothing else, on [`Self::LayerRefused`]'s standing.
    AttributeRefused { detail: String },
    /// A column of this name exists with a different identity → HTTP **409**, no effect
    /// (`ingest.md` §1.1: a part present and different). Separate from [`Self::AttributeRefused`]
    /// because the remedy differs: a refused declaration is corrected and resent, and a held name
    /// is one the caller cannot have under another identity, since a column's width and
    /// placement are baked into every row (`per-point-attributes.md` §2.2).
    AttributeConflict { detail: String },
    /// A vocabulary of this name exists with a different identity, or a value of this key is held
    /// with a different property → HTTP **409**, no effect (`ingest.md` §1.1: a part present and
    /// different). Separate from [`Self::VocabularyRefused`] on
    /// [`Self::AttributeConflict`]'s rule: a refused declaration is corrected and resent, and a
    /// held identity is one the caller cannot have — a value's key and code are baked into every
    /// row that carries them, and its properties are supplied once with the value.
    VocabularyConflict { detail: String },
    /// A row would give an item a value of a unique column that another live or suppressed item
    /// holds, or a column declared unique already holds a value twice → HTTP **409**, no effect.
    /// The detail names the values and, for an ingest, the holders' `mosaica_id`s; never an entity
    /// id.
    UniqueTaken { detail: String },
    /// What the handler resolved a batch against has changed since: an item a row names was
    /// deleted or joined to the row's view, or a value a row carries has a new holder, or a fold
    /// freed ids since a command's names were resolved, or an edit moved an item a command names
    /// while a fold retired entities. Nothing took effect, and the names are resolved again.
    Stale,
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
            ExecError::VocabularyRefused { detail } | ExecError::VocabularyConflict { detail } => {
                write!(f, "{detail}")
            }
            ExecError::LayerRefused { detail } => write!(f, "{detail}"),
            ExecError::PartConflict { detail } => write!(f, "{detail}"),
            ExecError::ViewRefused { detail }
            | ExecError::ViewConflict { detail }
            | ExecError::AttributeRefused { detail }
            | ExecError::AttributeConflict { detail }
            | ExecError::ViewUnknown { detail }
            | ExecError::UniqueTaken { detail } => write!(f, "{detail}"),
            ExecError::Stale => write!(
                f,
                "the items this batch names changed while it was checked; send it again"
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

#[cfg(test)]
mod tests {
    use super::*;

    fn row() -> UnallocatedRow {
        UnallocatedRow {
            view: "default".to_string(),
            join: None,
            descriptors: vec![b"dept:eng".to_vec(), b"region:emea".to_vec()],
            x: 1.5,
            y: -2.5,
            scalars: vec![WalScalar::U64(7), WalScalar::Utf8("s".into())],
            scoped: Vec::new(),
            terms: vec![TermId::new(9), TermId::new(2)],
        }
    }

    /// The conversion the executor performs, end to end: rows in, `PendingItem`s to the
    /// allocator, IDs back by position, `WalRow`s out. Every `WalRow` field must come from the
    /// `UnallocatedRow` or from the allocator — if a field could only be filled in with a default,
    /// the type does not line up with `WalRow` and the conversion is not mechanical.
    ///
    /// The pair is a **round trip** rather than two independent reads, so this asserts the
    /// round trip: every field must arrive on the far side, including `terms`, which travels
    /// *through* the `PendingItem` rather than staying in the row. The
    /// hollow interval between the two calls is asserted too — it is the price of not cloning, and a
    /// reader who does not know about it would be surprised by it exactly once.
    #[test]
    fn an_unallocated_row_carries_everything_a_wal_row_needs_except_the_id() {
        let original = row();
        let mut row = original.clone();

        let mut pending = row.take_pending();
        assert_eq!(pending.terms, original.terms);
        assert!(
            pending.entity_id.is_none(),
            "the whole point: no id until the executor assigns one"
        );
        assert!(
            row.terms.is_empty(),
            "the terms MOVED: the row is hollow until `into_wal_row_with` reunites them"
        );

        // What `assign_sorted` does, at one item's scale.
        pending.entity_id = Some(EntityId::new(41));

        let (wal_row, terms) = row.into_wal_row_with(pending);
        assert_eq!(wal_row.entity_id, EntityId::new(41));
        assert_eq!(wal_row.descriptors, original.descriptors);
        assert_eq!(wal_row.x, original.x);
        assert_eq!(wal_row.y, original.y);
        assert_eq!(wal_row.scalars, original.scalars);
        assert_eq!(
            terms, original.terms,
            "and the resolved terms come back too"
        );
    }

}
