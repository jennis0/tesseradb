//! What a handler submits to the write executor, and the channel it is answered on.
//!
//! A command carries its own [`Reply`], so what answers it is decided by which command it is:
//! [`Command::DropLayer`] can be answered with `()` and nothing else, [`Command::RegisterLayer`]
//! with the layer's entity and nothing else. There is no answer vocabulary to match against and no
//! arm for the answer a command cannot receive.
//!
//! **It lives here rather than in `tessera-lifecycle`** because nothing outside this module builds
//! a command or reads a reply. What does live there is the half a command carries —
//! [`tessera_lifecycle::UnallocatedRow`], the requests, the errors — which is entity-space and
//! store-free, and which `tessera-server` names when it maps a failure to a status.

use std::sync::mpsc::{Receiver, SyncSender};

use tessera_lifecycle::command::{
    AttributeRequest, BatchArtifacts, DeclaredValue, ExecError, MembershipGrown, SubmitError,
    UnallocatedRow, ValuesRequest, VocabularyRequest,
};
use tessera_lifecycle::membership::{IncomingArtifact, IncomingGrowth};
use tessera_lifecycle::wal::{ChangeOp, PlainViewDeclaration, ViewGroupDeclaration};
use tessera_types::EntityId;

use super::{AcceptError, PublishedBatch, ValuesReceipt};

/// The executor's end of one command's reply channel.
///
/// **A reply is a promise that the effect is in force**, not that the command was queued: the
/// executor sends it strictly *after* the generation carrying the effect has been swapped in
/// (`append → fsync → apply → swap → ack`). The fail-open this ordering exists to prevent is an
/// ack that precedes the swap, letting a client observe a 200 for a suppression that is not yet in
/// force.
pub(crate) struct Reply<T>(SyncSender<Result<T, ExecError>>);

impl<T> Reply<T> {
    /// The two halves of one command's reply: the one that travels with the command, and the one
    /// the submitter waits on.
    pub(crate) fn channel() -> (Reply<T>, Pending<T>) {
        let (tx, rx) = std::sync::mpsc::sync_channel(1);
        (Reply(tx), Pending(rx))
    }

    /// A caller that has gone away is not an error: the effect stands either way.
    pub(super) fn ack(&self, value: T) {
        let _ = self.0.send(Ok(value));
    }

    /// The caller maps the variant to a status code; see [`ExecError`] for the mapping and for
    /// which variants still applied something.
    pub(super) fn fail(&self, error: ExecError) {
        let _ = self.0.send(Err(error));
    }
}

/// An enqueued command whose reply has not been collected yet.
///
/// Holding one of these is what lets a caller with N commands have all N in the executor's queue at
/// once, which is the only condition under which the deny lane's group commit has anything to
/// gather ([`super::LifecycleHandle::enqueue`]).
pub(crate) struct Pending<T>(Receiver<Result<T, ExecError>>);

impl<T> Pending<T> {
    /// Block until the executor answers.
    ///
    /// A dropped reply means the executor died **holding this command** — never `Ok`. Answering
    /// anything else here is the false-202 [`SubmitError`]'s own doc calls the worst available
    /// outcome.
    ///
    /// [`SubmitError::ReceiptLost`] because the ack is the **last** step:
    /// `append → fsync → apply → swap → ack` (`Executor::commit_denies`), so a death after the
    /// swap leaves a durable, in-force suppression with no reply. Reporting that as "nothing was
    /// submitted" is how an operator comes to believe an item is still visible when it is not.
    pub(crate) fn wait(self) -> Result<Result<T, ExecError>, SubmitError> {
        self.0.recv().map_err(|_| SubmitError::ReceiptLost)
    }

    /// The same wait, folded into the one error a handler answers with.
    pub(crate) fn accept(self) -> Result<T, AcceptError> {
        self.wait()?.map_err(AcceptError::Exec)
    }
}

/// What one accepted ingest batch did.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Ingested {
    /// One `EntityId` per submitted row, **in the caller's submitted row order** — not in the
    /// signature-sorted order the IDs were assigned in. The handler turns each into a
    /// `tessera_id` for the response (contracts §3.4 r6), which is the only reason an entity ID is
    /// materialised outside the engine at all (I10).
    pub(crate) entity_ids: Vec<EntityId>,
    /// How many artifacts this batch's membership column **created** — a key no artifact held, on a
    /// layer whose `value_set` is open (`artifacts-from-points.md` §3). Zero for every batch that
    /// named none, which is every batch that carries no membership column and every one whose keys
    /// all existed.
    ///
    /// **Reported because minting is not undoable.** A typo creates a permanent object rather than
    /// being refused, which is the trade an open layer makes knowingly; the mitigation is that the
    /// caller who made it is told, in the same 200 that accepted the rows.
    ///
    /// **A replayed batch reports zero**, and that is the honest reading: the count is what *this
    /// submission* created, and a duplicate batch id creates nothing.
    pub(crate) minted: u64,
}

/// What one vocabulary declaration did.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct VocabularyDeclared {
    /// Whether a vocabulary of that name already carried this identity, in which case the
    /// declaration's values were applied to it as a page.
    pub(crate) existing: bool,
    /// How many of the request's values were novel, the rest having been bound already.
    pub(crate) added: u64,
    /// How many held values the request gave a title differing from the one they carried.
    pub(crate) titles: u64,
}

/// What one page of vocabulary values did. All three counts are bounded by the caller's own page.
///
/// **`titles` is reported because a title upsert overwrites.** The count is what tells a caller how
/// much of their page changed a name a client draws, where `added` and `existing` say only which
/// keys were bound.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct VocabularyValues {
    /// Values that were novel and drew a code.
    pub(crate) added: u64,
    /// Values that were already bound.
    pub(crate) existing: u64,
    /// Held values whose title this page replaced.
    pub(crate) titles: u64,
}

/// One unit of work for the write executor, and the channel it is answered on.
///
/// Ingest and change are the whole vocabulary. A flush and a compaction fold are specified as
/// further commands and would arrive **additively** — new variants, not a changed shape for these
/// two.
/// **⊘ Specified, not implemented.** Neither exists, so nothing today submits anything but these.
pub(crate) enum Command {
    /// An accepted `/control/ingest` batch, rows not yet allocated (see
    /// [`tessera_lifecycle::UnallocatedRow`]).
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
        reply: Reply<Ingested>,
    },
    /// One accepted `/control/changes` entry.
    ///
    /// **Addressed by entity, whatever the caller supplied.** The handler resolves an
    /// `external_id` through the live map and the bundle sidecar, or inverts a `tessera_id`, and
    /// carries the result — so the executor re-resolves nothing inside its critical section, and
    /// the record it appends names the entity rather than an identifier whose meaning depends on
    /// a key (`WalRecord::ChangeByEntity`). An item ingested without an external id is addressable
    /// only this way, which is the hole addressing by entity closes.
    ///
    /// Nothing comes back: the caller named the item.
    Change {
        entity: EntityId,
        op: ChangeOp,
        reply: Reply<()>,
    },
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
    ///
    /// The layer's own entity comes back, so the handler can hand back its `tessera_id` — the only
    /// address by which a caller can later suppress the layer, since an entity id never crosses the
    /// boundary (**I10**).
    RegisterLayer {
        declaration: Box<tessera_types::layer::LayerDeclaration>,
        reply: Reply<EntityId>,
    },
    /// Drop an annotation layer, tombstoning its name for ever. Nothing comes back: the caller
    /// named it.
    DropLayer { name: String, reply: Reply<()> },
    /// Create a view of a view group (`views.md` §3.2).
    ///
    /// **The record travels unvalidated**, in the same shape and for the same reason as
    /// [`Command::RegisterLayer`]'s declaration: the checks that decide a key is free — and the
    /// ordinal that follows them — read state only the executor may write, so a handler that
    /// validated first could be overtaken by a create of the same key between its check and the
    /// enqueue, and would then have acked two views onto one key.
    ///
    /// Nothing comes back: the caller named the group and the key, and the key is the view's only
    /// address (decision 0113).
    CreateView {
        group: String,
        key: String,
        /// The gate's labels, each one term (decision 0132); `None` is `public`.
        visibility: Option<Vec<String>>,
        metadata: std::collections::BTreeMap<String, tessera_types::view::ViewMetadataValue>,
        reply: Reply<()>,
    },
    /// Drop a view of a view group, tombstoning its key for ever (`views.md` §3.4).
    ///
    /// `delete_dangling` is **sugar and nothing else**: at the drop the executor computes the
    /// entities of this view that hold a row in no other view — the commit-window buffer included
    /// — and submits them as *ordinary* deletions, which enter the overlay and retire at the fold
    /// like any deletion (Rule F, write-path §5.4). It is not a second retirement route and must
    /// not become one; a drop that removed an entity any other way would be the fail-open the two
    /// removal rules exist to prevent.
    ///
    /// The answer is how many entities `delete_dangling` submitted for deletion — **reported
    /// because the operation is not undoable**, on the rule [`Ingested::minted`] is reported by,
    /// and `0` for a drop that did not ask for it.
    DropView {
        group: String,
        key: String,
        delete_dangling: bool,
        reply: Reply<u64>,
    },
    /// Declare an attribute column while the service runs (`PUT /control/attributes`,
    /// `ingest.md` §1.3, §6.3).
    ///
    /// **The request travels unvalidated**, on [`Command::RegisterLayer`]'s rule: whether the name
    /// is free, whether a column of that name already carries this identity, and which width a
    /// vocabulary no column named before takes, all read the served schema and the live bindings,
    /// which only the executor may move between a check and an apply. Boxed for the reason the
    /// layer declaration is.
    ///
    /// The answer is whether an identical declaration met the column that already carries it.
    /// Nothing else: the name is the column's only address, on every surface that names one.
    DeclareAttribute {
        request: Box<AttributeRequest>,
        reply: Reply<bool>,
    },
    /// Declare a vocabulary while the service runs (`PUT /control/vocabularies/{name}`,
    /// `ingest.md` §1.3). On the executor for [`Command::DeclareAttribute`]'s reason: whether the
    /// name is free and whether a held vocabulary carries this identity read state only the
    /// executor may move between a check and an apply. Boxed as the attribute request is.
    DeclareVocabulary {
        request: Box<VocabularyRequest>,
        reply: Reply<VocabularyDeclared>,
    },
    /// Declare a view group while the service runs (`PUT /control/view_groups/{name}`,
    /// `ingest.md` §1.3). On the executor for [`Command::DeclareAttribute`]'s reason: whether the
    /// name is free, and what a held group's identity is, read state only the executor may move
    /// between a check and an apply.
    ///
    /// The answer is whether an identical declaration met the group that already carries that name.
    /// Nothing else: the name is the group's only address.
    CreateViewGroup {
        declaration: Box<ViewGroupDeclaration>,
        reply: Reply<bool>,
    },
    /// Create a plain view while the service runs (`PUT /control/views/{name}`, `ingest.md` §1.3
    /// and §10, R9), on [`Command::CreateViewGroup`]'s rule and answered on its terms.
    CreatePlainView {
        declaration: Box<PlainViewDeclaration>,
        reply: Reply<bool>,
    },
    /// A page of values for a vocabulary that already exists
    /// (`PATCH /control/vocabularies/{name}/values`, `ingest.md` §1.3).
    ///
    /// **The codes are not here**, and cannot be: a code is drawn on the executor at the moment
    /// the binding becomes durable, and a caller who supplied one would be the minting authority
    /// for a space the server owns (per-point-attributes §3.1).
    MintVocabularyValues {
        vocabulary: String,
        values: Vec<DeclaredValue>,
        reply: Reply<VocabularyValues>,
    },
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
    ///
    /// The answer carries the artifacts' **entities, which the handler turns into `tessera_id`s —
    /// never the ordinals.** An ordinal is a position in a dense level, so a caller holding two of
    /// them learns how many artifacts sit between; across two principals it is a corpus-wide count
    /// over objects one of them may not see, which is C8's row. The `tessera_id` is the only
    /// artifact address that crosses the wire.
    PublishArtifacts {
        layer: String,
        level: u32,
        artifacts: Vec<IncomingArtifact>,
        reply: Reply<PublishedBatch>,
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
    ///
    /// One receipt comes back per join the caller submitted, in the caller's order.
    GrowMemberships {
        layer: String,
        level: u32,
        joins: Vec<IncomingGrowth>,
        reply: Reply<Vec<MembershipGrown>>,
    },
    /// An accepted `POST /control/values` batch: attribute values for entities that already
    /// exist, filled per cell under the fill rule (`ingest.md` §1.1, §1.4).
    ///
    /// **The comparison travels unmade**, on [`Command::Ingest`]'s rule and the join arm's
    /// (decision 0116): whether a cell is absent, holds the identical value or holds a different
    /// one is read from the buffer and the flushed homes, which only the executor may move
    /// between a check and an apply. A handler that compared first could be overtaken by the
    /// window that writes the cell and would then fill it twice.
    ///
    /// **Entities, not identifiers.** The handler resolves each row's `external_id` or inverts
    /// its `tessera_id` at the boundary, on [`Command::Change`]'s rule, so no blinded identifier
    /// reaches the executor or the log (**I10**).
    ///
    /// It rides the bounded, sheddable lane ([`Command::is_never_shed`]): a values batch refused
    /// for load is backpressure and the caller retries, nothing having been filled.
    ///
    /// **Boxed** so one large variant does not set the size of every command in the queue, on
    /// [`Command::RegisterLayer`]'s rule.
    Values {
        request: Box<ValuesRequest>,
        reply: Reply<ValuesReceipt>,
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
    pub(crate) fn is_never_shed(&self) -> bool {
        matches!(self, Command::Change { .. })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The lane asymmetry, asserted on the vocabulary itself: every `/control/changes` command
    /// takes the never-shed lane regardless of op — including `Unsuppress`, the one a reader is
    /// most likely to assume is ordinary work — and ingest never does.
    #[test]
    fn every_change_rides_the_never_shed_lane_and_no_ingest_does() {
        for op in [ChangeOp::Delete, ChangeOp::Suppress, ChangeOp::Unsuppress] {
            let (reply, _pending) = Reply::channel();
            let cmd = Command::Change {
                entity: EntityId::new(1),
                op,
                reply,
            };
            assert!(cmd.is_never_shed(), "{op:?} must not be sheddable for load");
        }
        let (reply, _pending) = Reply::channel();
        let ingest = Command::Ingest {
            rows: Vec::new(),
            batch_id: "b".into(),
            body_hash: [0u8; 32],
            artifacts: Default::default(),
            reply,
        };
        assert!(!ingest.is_never_shed());
    }
}
