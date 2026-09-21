use super::*;

/// How often a degraded node retries WAL recovery. No caller waits on it: a degraded node still
/// answers denies at once.
pub(super) const WAL_RECOVERY_POLL_INTERVAL: std::time::Duration = std::time::Duration::from_secs(1);

/// Why one [`Executor::append_and_sync`] run did not reach durability. The two are told apart
/// because the deny lane retries a sync and not a torn append.
pub(super) enum Undurable {
    /// The record at this index into the run's input could not be appended.
    Append { at: usize, error: WalError },
    /// Every record was appended and none of them is durable.
    Fsync(WalError),
}

impl Undurable {
    pub(super) fn into_error(self) -> WalError {
        match self {
            Undurable::Append { error, .. } | Undurable::Fsync(error) => error,
        }
    }
}

/// The executor's WAL, where the log stood at last rotation, and when the gauge was last walked.
pub(in crate::write) struct ExecutorLog {
    pub(super) wal: ExecutorWal,
    /// So a tick can tell whether the log has grown since: the deny-only regime's rotation trigger.
    position_at_last_rotation: u64,
    /// When [`ExecutorLog::sample_due`] last let a walk begin, or `None` before the first one.
    last_sample: Option<std::time::Instant>,
    /// Published as [`WalGauge::samples`], so a reader can tell a fresh reading from a stale one.
    samples: u64,
}

impl ExecutorLog {
    /// Freshly started, so it rotates nothing until something is appended in this run.
    pub(in crate::write) fn new(wal: ExecutorWal, position: u64) -> Self {
        ExecutorLog {
            wal,
            position_at_last_rotation: position,
            last_sample: None,
            samples: 0,
        }
    }

    /// Whether anything has been appended since the last rotation.
    fn has_grown(&self) -> bool {
        self.wal.position() != self.position_at_last_rotation
    }

    /// Takes the position after the rotation, so the next growth check counts only later appends.
    fn mark_rotated(&mut self) {
        self.position_at_last_rotation = self.wal.position();
    }

    /// The walk's number if one may begin now, stamped before the walk so a slow one shortens the
    /// next interval rather than pushing it out.
    fn sample_due(&mut self, period: std::time::Duration) -> Option<u64> {
        if let Some(last) = self.last_sample {
            if last.elapsed() < period {
                return None;
            }
        }
        self.last_sample = Some(std::time::Instant::now());
        self.samples += 1;
        Some(self.samples)
    }
}

/// Which of a failed run's answers carries the real error; every other gets
/// [`WalError::Poisoned`], what the WAL will in fact return for every call after the failure.
pub(super) struct Blame {
    real: Option<WalError>,
    blamed: usize,
}

impl Blame {
    pub(super) fn new(blamed: usize, error: WalError) -> Self {
        Blame {
            real: Some(error),
            blamed,
        }
    }

    /// The error answer `index` is owed.
    pub(super) fn at(&mut self, index: usize) -> WalError {
        if index == self.blamed {
            self.real.take().unwrap_or(WalError::Poisoned)
        } else {
            WalError::Poisoned
        }
    }
}

impl Executor {
    /// If the WAL is degraded and the degradation is one a discard can end, end it: every caller
    /// has already been told its write is not durable, so making it durable now would be
    /// fail-open. A torn append does not recover; such a node stays `WalPoisoned` until restarted.
    pub(super) fn recover_wal(&mut self) {
        if !self.log.wal.is_poisoned() {
            return;
        }
        if !self.log.wal.is_recoverable() {
            return;
        }
        if self.log.wal.discard_undurable().is_ok() {
            // What the apply-anyway rule applied is now in force with no record behind it: the
            // overlay is diverged, so no flush publishes and no WAL rotates until a restart.
            if !self.health.overlay_diverged.swap(true, Ordering::SeqCst) {
                tracing::error!(
                    "ALARM: this node recovered its WAL in process, so its overlay now holds \
                     dispositions no durable record backs. It keeps serving and keeps applying \
                     denies, but publishes NO flush and rotates NO WAL until restarted; ingest \
                     stops becoming visible. Restart this node."
                );
            }
        }
        self.observe_wal();
    }

    /// Append `records` in order and fsync once: the one path to durability. Each position is read
    /// before its own append, since that is the bound rotation must not reclaim past. On failure
    /// nothing the caller prepared is in force, and its reserved ids stay spent.
    pub(super) fn append_and_sync(
        &mut self,
        records: &[&WalRecord],
        laps: Option<StageMark>,
    ) -> std::result::Result<Vec<u64>, Undurable> {
        let mut positions = Vec::with_capacity(records.len());
        let mut failed = None;
        for (at, record) in records.iter().enumerate() {
            let before = self.log.wal.position();
            if let Err(error) = self.log.wal.append(record) {
                failed = Some(Undurable::Append { at, error });
                break;
            }
            positions.push(before);
        }
        let mark = laps.map(|mark| self.health.lap(WriteStage::WalAppend, mark));
        if failed.is_none() {
            if let Err(error) = self.log.wal.fsync() {
                failed = Some(Undurable::Fsync(error));
            }
        }
        if let Some(mark) = mark {
            self.health.lap(WriteStage::WalFsync, mark);
        }
        self.observe_wal();
        match failed {
            Some(failure) => Err(failure),
            None => Ok(positions),
        }
    }

    /// [`Self::append_and_sync`] for a command that answers one caller: the failure is an
    /// `ExecError` and the operator gets the line naming `what`.
    pub(super) fn make_durable(
        &mut self,
        records: &[&WalRecord],
        what: &str,
    ) -> Result<Vec<u64>, ExecError> {
        self.append_and_sync(records, None).map_err(|failure| {
            let e = failure.into_error();
            tracing::error!(error = %e, "ALARM: {what} could not be made durable; none of it is in force");
            ExecError::Wal(e)
        })
    }

    /// Reclaim what the publication just made redundant, after the generation swap and never
    /// before it. A poisoned WAL, or an overlay diverged from its durable WAL, rotates nothing:
    /// writing a snapshot from a diverged overlay would make a never-acked deny permanent.
    pub(super) fn rotate_wal(&mut self) {
        if self.log.wal.is_poisoned() {
            return;
        }
        if !self.may_publish() {
            tracing::warn!(
                "this node's overlay has diverged from its durable WAL, so it rotates nothing; \
                 the log grows until an operator restarts it"
            );
            return;
        }

        let generation = self.generation.load();
        // A stepped-down node reclaims nothing: its WAL members are its only recovery material.
        if generation
            .bundle
            .partitions
            .values()
            .any(|p| p.stepped_down())
        {
            tracing::warn!(
                "a partition is stepped down, so this node rotates nothing; the log grows until \
                 the damaged newest manifest is repaired"
            );
            return;
        }
        let reclaim_below = match generation.buffer.oldest_wal_pos() {
            // Nothing buffered: the whole durable prefix is reclaimable.
            None => self.log.wal.position(),
            Some(Some(oldest)) => oldest,
            // A buffered row of unknown position pins the log: fail-safe rather than guessing.
            Some(None) => 0,
        };
        // The oldest unpublished artifact record pins the log too: a membership has no home
        // outside the WAL.
        let reclaim_below = match self.live.artifacts_oldest_wal_pos() {
            Some(oldest) => reclaim_below.min(oldest),
            None => reclaim_below,
        };

        let snapshot = generation.overlay.snapshot();
        match self.log.wal.rotate(&snapshot, reclaim_below) {
            Ok(deleted) => {
                self.log.mark_rotated();
                if !deleted.is_empty() {
                    // The idempotency index follows the log it caches, or a replay could be
                    // answered as unknown after a restart.
                    let forgotten = self.live.forget_batches_below(self.log.wal.retained_from());
                    tracing::info!(
                        members = ?self.log.wal.members(),
                        reclaimed = ?deleted,
                        forgotten_batch_ids = forgotten,
                        "WAL members reclaimed below the oldest unconsumed row"
                    );
                }
            }
            Err(e) => {
                tracing::warn!(error = %e, "the WAL did not rotate; the log grows until it does");
            }
        }
    }

    /// The deny-only regime's rotation, without which a node that takes denies but never flushes
    /// would replay an unbounded log at restart. Gated on growth, so an idle node rotates nothing.
    pub(super) fn rotate_if_grown(&mut self) {
        if !self.log.has_grown() {
            return;
        }
        self.rotate_wal();
    }

    /// Read the WAL's size and its rotation bound into [`ExecutorHealth::wal_gauge`], at most once
    /// per tick period. Rotates nothing, compares nothing against a limit, returns no decision.
    pub(super) fn sample_wal_gauge(&mut self) {
        let period = std::time::Duration::from_secs(self.deps.flush_max_age_secs);
        let Some(samples) = self.log.sample_due(period) else {
            return;
        };
        let position = self.log.wal.position();
        let pin = self.live.with_artifacts(|store| store.wal_pin());
        self.health.record_wal_gauge(WalGauge {
            members: self.log.wal.member_count(),
            bytes: self.log.wal.disc_bytes(),
            position,
            pin,
            pin_span_bytes: pin.map_or(0, |(_, pos)| position.saturating_sub(pos)),
            samples,
        });
    }

    /// Mirror the WAL's own poison flag into the posture, asked fresh rather than remembered, so
    /// it cannot drift from the thing it describes.
    pub(super) fn observe_wal(&self) {
        self.health.mirror_wal(self.log.wal.is_poisoned());
    }
}
