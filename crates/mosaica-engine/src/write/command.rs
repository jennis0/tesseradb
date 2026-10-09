//! What a handler submits to the write executor, and the channel it is answered on.
//!
//! A command carries its own [`Reply`], so what answers it is decided by which command it is:
//! [`Command::DropLayer`] can be answered with `()` and nothing else, [`Command::RegisterLayer`]
//! with the layer's entity and nothing else. There is no answer vocabulary to match against and no
//! arm for the answer a command cannot receive.
//!
//! It lives here rather than in `mosaica-lifecycle` because nothing outside this module builds a
//! command or reads a reply. What does live there is the half a command carries
//! ([`mosaica_lifecycle::UnallocatedRow`], the requests, the errors), which is entity-space and
//! store-free, and which `mosaica-server` names when it maps a failure to a status.

use std::sync::mpsc::{Receiver, SyncSender};
use std::sync::Arc;

use mosaica_lifecycle::command::{
    AttributeRequest, BatchArtifacts, DeclaredValue, ExecError, MembershipGrown, SubmitError,
    UnallocatedEdit, UnallocatedRow, VocabularyRequest,
};
use mosaica_lifecycle::membership::{IncomingArtifact, IncomingGrowth};
use mosaica_lifecycle::wal::{ChangeOp, PlainViewDeclaration, ViewGroupDeclaration};
use mosaica_types::EntityId;

use super::{AcceptError, ExecutorHealth, PublishedBatch};

/// Where a command is answered. Every answer goes out through [`Reply::ack`] or [`Reply::fail`],
/// so under fault injection every command's answer passes the `BeforeAck` pause and is recorded
/// as a step, whichever handler sends it.
pub(crate) struct Reply<T> {
    tx: SyncSender<Result<T, ExecError>>,
    /// Set on a work-lane job's reply: the job is counted completed just before it is answered, so
    /// a caller that reads the stats after its answer sees its own job.
    work: Option<Arc<ExecutorHealth>>,
    #[cfg(feature = "fault-injection")]
    faults: Faults,
}

/// The fault switchboard a reply reports to, where one is installed.
#[cfg(feature = "fault-injection")]
pub(crate) type Faults = Option<std::sync::Arc<mosaica_lifecycle::faults::FaultSwitchboard>>;

impl<T> Reply<T> {
    pub(crate) fn channel(
        work: Option<Arc<ExecutorHealth>>,
        #[cfg(feature = "fault-injection")] faults: Faults,
    ) -> (Reply<T>, Pending<T>) {
        let (tx, rx) = std::sync::mpsc::sync_channel(1);
        let reply = Reply {
            tx,
            work,
            #[cfg(feature = "fault-injection")]
            faults,
        };
        (reply, Pending(rx))
    }

    /// Answers the command. A caller that has gone away is not an error: the effect stands.
    pub(super) fn ack(self, value: T) {
        self.count_work();
        #[cfg(feature = "fault-injection")]
        if let Some(faults) = &self.faults {
            use mosaica_lifecycle::faults::{PauseAction, PauseSite, Step};
            if let Some(PauseAction::Panic) = faults.pause_point(PauseSite::BeforeAck) {
                panic!("fault-injection: executor panicked at the BeforeAck pause point");
            }
            faults.record(Step::Ack);
        }
        let _ = self.tx.send(Ok(value));
    }

    pub(super) fn fail(self, error: ExecError) {
        self.count_work();
        #[cfg(feature = "fault-injection")]
        if let Some(faults) = &self.faults {
            faults.record(mosaica_lifecycle::faults::Step::Ack);
        }
        let _ = self.tx.send(Err(error));
    }

    fn count_work(&self) {
        if let Some(health) = &self.work {
            health.note_work_finished();
        }
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
    /// A dropped reply means the executor died holding this command: never `Ok`, and answered as
    /// [`SubmitError::ReceiptLost`] rather than "nothing was submitted", because the ack is the
    /// last step (`append`, `fsync`, `apply`, `swap`, `ack` in `Executor::commit_denies`), so a
    /// death after the swap leaves a durable, in-force suppression with no reply.
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
    /// One per row of the request, in request order.
    pub(crate) receipt: Vec<mosaica_lifecycle::RowReceipt>,
    /// How many artifacts this batch's membership columns created: a key no artifact held, on a
    /// layer whose `value_set` is open. Reported because a minted artifact cannot be undone.
    pub(crate) minted: u64,
    /// How many memberships this batch's membership columns added, to artifacts it created and to
    /// held ones alike.
    pub(crate) joined: u64,
    /// The batch id was accepted earlier with this body, and `receipt` is that acceptance's.
    pub(crate) replayed: bool,
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
/// `titles` is reported because a title upsert overwrites: the count tells a caller how much of
/// their page changed a name a client draws, where `added` and `existing` say only which keys
/// were bound.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct VocabularyValues {
    /// Values that were novel and drew a code.
    pub(crate) added: u64,
    /// Values that were already bound.
    pub(crate) existing: u64,
    /// Held values whose title this page replaced.
    pub(crate) titles: u64,
}

/// What a view drop did beside dropping the view.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ViewDropped {
    /// Items the drop deleted: those it left with a row in no view.
    pub deleted: u64,
}

/// One unit of work for the write executor, and the channel it is answered on.
///
/// A command carries what the caller sent, unvalidated and unallocated. Whether a name or key is
/// free, which ids or ordinals or codes to allocate, and whether a batch id was seen before all
/// read state only the executor may write, so they are decided there: a handler that checked
/// first could be overtaken between its check and the enqueue, and two callers would be
/// acknowledged onto one name. Items and members are addressed by entity, which the handler
/// resolves at the boundary, because a `tessera_id` means nothing without its key. Large payloads
/// are boxed so one variant does not set the size of every command in the queue.
/// One `/control/ingest` batch as its handler resolved it.
pub(crate) struct IngestSubmission {
    /// The rows the batch writes: items it creates, and items it adds to its view in place.
    pub(crate) rows: Vec<UnallocatedRow>,
    /// The items the batch moves to new entities.
    pub(crate) edits: Vec<SubmittedEdit>,
    /// One per row of the request: the written row it became, or the item it left unchanged.
    pub(crate) slots: Vec<mosaica_lifecycle::Slot>,
    /// The unique values the created rows set, as `(declared position, key widened)`.
    pub(crate) keys: Vec<(u16, u128)>,
    pub(crate) batch_id: String,
    pub(crate) body_hash: [u8; 32],
    /// The artifacts the written rows name in a column named for a layer. Resolved and grown
    /// when the window closes, in the same commit as the rows.
    pub(crate) artifacts: BatchArtifacts,
    /// The live unique entries' sequence number the handler resolved the batch at; the
    /// executor re-checks the created rows' values against the entries added since.
    pub(crate) unique_seq: u64,
    /// The request rows creating an item indexed under more than
    /// [`mosaica_authz::MAX_KEYS_PER_ITEM`] keys.
    pub(crate) over_bound: Vec<u32>,
}

/// One item an ingest batch edits, and what the handler read of it: the executor refuses the
/// edit as stale where the item's rows have moved since.
pub(crate) struct SubmittedEdit {
    pub(crate) edit: UnallocatedEdit,
    /// Every view the old entity held a row in, as the handler read it.
    pub(crate) held_views: Vec<String>,
}

pub(crate) enum Command {
    /// An accepted `/control/ingest` batch, resolved by its handler. `batch_id` and `body_hash`
    /// are the idempotency key. The answer is what each row of the request became.
    Ingest {
        submission: IngestSubmission,
        reply: Reply<Ingested>,
    },
    /// One accepted `/control/changes` request, applied whole. The only command on the deny lane
    /// ([`Command::is_never_shed`]).
    Changes {
        changes: Vec<(EntityId, ChangeOp)>,
        stamp: crate::edited::Stamp,
        reply: Reply<()>,
    },
    /// Register an annotation layer. The answer is the layer's own entity, which the handler turns
    /// into the `tessera_id` a caller later suppresses the layer by.
    RegisterLayer {
        declaration: Box<mosaica_types::layer::LayerDeclaration>,
        reply: Reply<EntityId>,
    },
    /// Drop an annotation layer. Its name is never issued again.
    DropLayer { name: String, reply: Reply<()> },
    /// Create a view of a view group.
    CreateView {
        group: String,
        key: String,
        /// The gate's labels, each one term; `None` is `public`.
        visibility: Option<Vec<String>>,
        metadata: std::collections::BTreeMap<String, mosaica_types::view::ViewMetadataValue>,
        reply: Reply<()>,
    },
    /// Drop a view of a view group, deleting the items it leaves with a row in no view. The
    /// deletions retire at the fold like any other.
    DropView {
        group: String,
        key: String,
        reply: Reply<ViewDropped>,
    },
    /// Declare an attribute column. The answer is whether an identical declaration already held the
    /// name.
    DeclareAttribute {
        request: Box<AttributeRequest>,
        reply: Reply<bool>,
    },
    /// Declare a vocabulary.
    DeclareVocabulary {
        request: Box<VocabularyRequest>,
        reply: Reply<VocabularyDeclared>,
    },
    /// Declare a view group. The answer is whether an identical declaration already held the name.
    CreateViewGroup {
        declaration: Box<ViewGroupDeclaration>,
        reply: Reply<bool>,
    },
    /// Create a plain view, answered as [`Command::CreateViewGroup`] is.
    CreatePlainView {
        declaration: Box<PlainViewDeclaration>,
        reply: Reply<bool>,
    },
    /// A page of values for a vocabulary that exists. The caller supplies no codes: the server
    /// owns the code space.
    MintVocabularyValues {
        vocabulary: String,
        values: Vec<DeclaredValue>,
        reply: Reply<VocabularyValues>,
    },
    /// Publish a batch of artifacts into one level of one layer. The answer carries their entities
    /// and never their ordinals: an ordinal is a position in a dense level, so two of them tell a
    /// caller how many artifacts lie between, including ones the caller may not see.
    PublishArtifacts {
        layer: String,
        level: u32,
        artifacts: Vec<IncomingArtifact>,
        stamp: crate::edited::Stamp,
        reply: Reply<PublishedBatch>,
    },
    /// Add entities to the memberships of artifacts that exist, each named by its key. The whole
    /// batch or none of it: a key naming no artifact refuses the command. One receipt per join, in
    /// the caller's order.
    GrowMemberships {
        layer: String,
        level: u32,
        joins: Vec<IncomingGrowth>,
        stamp: crate::edited::Stamp,
        reply: Reply<Vec<MembershipGrown>>,
    },
}

impl Command {
    /// Whether this command rides the never-refused deny lane: the unbounded queue the executor
    /// drains to empty before it touches work.
    ///
    /// The lane is chosen by endpoint, not by op. Every `/control/changes` entry takes it,
    /// including [`ChangeOp::Unsuppress`], because `/control/changes` cannot answer 429: batching a
    /// security operation for latency is acceptable, refusing one for load is not, and an
    /// `Unsuppress` shed for load leaves an item hidden that a caller was told to expect back.
    ///
    /// A sustained deny flood starves ingest completely, and this queue is unbounded in memory.
    ///
    /// A new variant defaults to the bounded, sheddable lane: this is a `matches!` over one
    /// variant, so a `Flush` or a `Compact` is sheddable the moment it is added and nothing warns
    /// about it. That default is right for those two, a flush that cannot be admitted is
    /// backpressure working, but it is the wrong default for anything a caller is owed an
    /// unrefusable answer to. There is no `submit_deny` to reach for instead: the lane follows the
    /// command, and this function is the whole of the rule.
    pub(crate) fn is_never_shed(&self) -> bool {
        matches!(self, Command::Changes { .. })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The lane asymmetry, asserted on the vocabulary itself: every `/control/changes` command
    /// takes the never-shed lane regardless of op, including `Unsuppress`, the one a reader is
    /// most likely to assume is ordinary work, and ingest never does.
    #[test]
    fn every_change_rides_the_never_shed_lane_and_no_ingest_does() {
        for op in [ChangeOp::Delete, ChangeOp::Suppress, ChangeOp::Unsuppress] {
            let (reply, _pending) = Reply::channel(
                None,
                #[cfg(feature = "fault-injection")]
                None,
            );
            let cmd = Command::Changes {
                changes: vec![(EntityId::new(1), op)],
                stamp: Default::default(),
                reply,
            };
            assert!(cmd.is_never_shed(), "{op:?} must not be sheddable for load");
        }
        let (reply, _pending) = Reply::channel(
                None,
                #[cfg(feature = "fault-injection")]
                None,
            );
        let ingest = Command::Ingest {
            submission: IngestSubmission {
                rows: Vec::new(),
                edits: Vec::new(),
                slots: Vec::new(),
                keys: Vec::new(),
                batch_id: "b".into(),
                body_hash: [0u8; 32],
                artifacts: Default::default(),
                unique_seq: 0,
                over_bound: Vec::new(),
            },
            reply,
        };
        assert!(!ingest.is_never_shed());
    }
}
