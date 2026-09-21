//! Work the executor hands to another thread and publishes when it comes back.

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{Receiver, Sender, SyncSender};
use std::sync::Arc;

/// One kind of background work (a flush, a merge, a coalesce, a fold, a suggestion rebuild). At
/// most one unit of a kind is in flight. A finished unit waits in the channel until the executor
/// thread takes it, because only that thread publishes, and rings the doorbell so that thread
/// wakes and takes it.
pub(in crate::write) struct Background<C> {
    in_flight: Arc<AtomicBool>,
    completed_pending: Arc<AtomicBool>,
    attempt: u64,
    done: Receiver<C>,
    submit: Sender<C>,
    bell: SyncSender<()>,
}

impl<C> Background<C> {
    pub(in crate::write) fn new(bell: SyncSender<()>) -> Self {
        Self::sharing(bell, Default::default(), Default::default())
    }

    /// As [`Self::new`], over flags something else also reads.
    pub(in crate::write) fn sharing(
        bell: SyncSender<()>,
        in_flight: Arc<AtomicBool>,
        completed_pending: Arc<AtomicBool>,
    ) -> Self {
        let (submit, done) = std::sync::mpsc::channel();
        Background {
            in_flight,
            completed_pending,
            attempt: 0,
            done,
            submit,
            bell,
        }
    }

    pub(super) fn in_flight(&self) -> bool {
        self.in_flight.load(Ordering::SeqCst)
    }

    /// A finished unit is in the channel and has not been taken.
    pub(super) fn completed_pending(&self) -> bool {
        self.completed_pending.load(Ordering::SeqCst)
    }

    /// Running, or finished and not yet published.
    pub(super) fn outstanding(&self) -> bool {
        self.in_flight() || self.completed_pending()
    }

    /// A number no earlier unit of this kind was given, for naming what the unit writes.
    pub(super) fn next_attempt(&mut self) -> u64 {
        self.attempt += 1;
        self.attempt
    }

    /// Marks a unit in flight and returns what its worker reports through.
    pub(super) fn start(&self) -> InFlight<C> {
        self.in_flight.store(true, Ordering::SeqCst);
        InFlight {
            in_flight: Arc::clone(&self.in_flight),
            completed_pending: Arc::clone(&self.completed_pending),
            submit: self.submit.clone(),
            bell: self.bell.clone(),
        }
    }

    /// Puts a finished unit in the channel from the executor thread itself.
    #[cfg(feature = "fault-injection")]
    pub(super) fn submit_now(&self, unit: C) {
        let _ = self.submit.send(unit);
    }

    /// The next finished unit, if one is waiting. Call [`Self::drained`] after taking any.
    pub(super) fn next_completed(&self) -> Option<C> {
        self.done.try_recv().ok()
    }

    /// Clears the pending flag after a drain that took something. Not after an empty one: a
    /// worker sets the flag before it sends, so clearing on an empty channel could erase the
    /// flag of a unit about to arrive and let the executor sleep a full tick past it.
    pub(super) fn drained(&self) {
        self.completed_pending.store(false, Ordering::SeqCst);
    }
}

/// A unit in flight. Dropping it clears the in-flight flag, so a worker that fails, panics or
/// never runs does not leave its kind blocked, and rings the doorbell, so the executor wakes to a
/// dispatch the cleared flag now allows.
pub(super) struct InFlight<C> {
    in_flight: Arc<AtomicBool>,
    completed_pending: Arc<AtomicBool>,
    submit: Sender<C>,
    bell: SyncSender<()>,
}

impl<C> InFlight<C> {
    /// Hands the finished unit to the executor and wakes it. The pending flag is set before the
    /// send, so the executor never sees neither "in flight" nor "pending" while a unit is in the
    /// channel, and the ring comes after it, so the wake it causes finds the flag already set.
    ///
    /// A full doorbell means a wake is already pending and a second token would add nothing.
    pub(super) fn complete(&self, unit: C) {
        self.completed_pending.store(true, Ordering::SeqCst);
        let _ = self.submit.send(unit);
        let _ = self.bell.try_send(());
    }
}

impl<C> Drop for InFlight<C> {
    fn drop(&mut self) {
        self.in_flight.store(false, Ordering::SeqCst);
        let _ = self.bell.try_send(());
    }
}
