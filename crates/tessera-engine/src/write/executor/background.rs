//! Work the executor hands to another thread and publishes when it comes back.

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{Receiver, Sender, SyncSender};
use std::sync::Arc;

/// Whether a unit of one kind is running, or finished and not yet published, read from its two
/// flags.
pub(crate) fn outstanding(in_flight: &AtomicBool, completed_pending: &AtomicBool) -> bool {
    in_flight.load(Ordering::SeqCst) || completed_pending.load(Ordering::SeqCst)
}

/// One kind of background work. At most one unit of a kind is in flight. A finished unit waits in
/// the channel until the executor thread takes it, since only that thread publishes.
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
        outstanding(&self.in_flight, &self.completed_pending)
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
            completed: false,
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

    /// Clears the pending flag; called only after a drain that took something.
    pub(super) fn drained(&self) {
        self.completed_pending.store(false, Ordering::SeqCst);
    }
}

/// A unit in flight. Dropping it uncompleted clears the in-flight flag, so a worker that fails,
/// panics or never runs does not leave its kind blocked, and rings the doorbell.
pub(super) struct InFlight<C> {
    completed: bool,
    in_flight: Arc<AtomicBool>,
    completed_pending: Arc<AtomicBool>,
    submit: Sender<C>,
    bell: SyncSender<()>,
}

impl<C> InFlight<C> {
    /// Hands the finished unit to the executor and wakes it. The unit stops being in flight here,
    /// after the pending flag is set, so it is outstanding throughout and no longer running once
    /// the executor can publish it. Consumed, so a worker still running afterwards cannot clear
    /// the flag of the next unit of its kind.
    pub(super) fn complete(mut self, unit: C) {
        self.completed = true;
        self.completed_pending.store(true, Ordering::SeqCst);
        self.in_flight.store(false, Ordering::SeqCst);
        let _ = self.submit.send(unit);
        let _ = self.bell.try_send(());
    }
}

impl<C> Drop for InFlight<C> {
    fn drop(&mut self) {
        if !self.completed {
            self.in_flight.store(false, Ordering::SeqCst);
            let _ = self.bell.try_send(());
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A unit that has handed back its result is no longer running, though it stays outstanding
    /// until the executor takes it, so a request arriving after its publication is not refused
    /// as if it were still running.
    #[test]
    fn a_completed_unit_is_outstanding_but_not_in_flight() {
        let (bell, _rung) = std::sync::mpsc::sync_channel(1);
        let background: Background<u32> = Background::new(bell);
        let unit = background.start();
        assert!(background.in_flight());
        unit.complete(7);
        assert!(!background.in_flight(), "the unit's work is done");
        assert!(background.outstanding(), "its result is not yet taken");
        assert_eq!(background.next_completed(), Some(7));
        background.drained();
        assert!(!background.outstanding());
    }

    /// Completing a unit consumes it, so nothing its worker does afterwards touches the flag of
    /// the next unit of its kind; dropping a unit uncompleted still clears its own.
    #[test]
    fn a_finished_worker_does_not_clear_the_next_units_flag() {
        let (bell, _rung) = std::sync::mpsc::sync_channel(2);
        let background: Background<u32> = Background::new(bell);
        background.start().complete(1);
        assert_eq!(background.next_completed(), Some(1));
        background.drained();
        let second = background.start();
        assert!(background.outstanding(), "the second unit is running");
        drop(second);
        assert!(!background.outstanding(), "a unit dropped uncompleted leaves nothing running");
    }
}
