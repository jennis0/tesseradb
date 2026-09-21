use super::*;

/// How often a degraded node retries WAL recovery. A second is short against the interval an
/// operator would take to notice, and long enough that a genuinely dead device is retried sixty
/// times a minute rather than continuously.
///
/// Not a latency bound on anything a caller sees: a degraded node still answers denies immediately.
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

/// Which of a failed run's answers carries the real error.
///
/// The one the failure belongs to gets it; every other gets [`WalError::Poisoned`], which is
/// precisely what its own append would have returned had it been attempted after the failure, and
/// what the WAL will in fact return for every subsequent call. Consumed as the answers are made,
/// so the real error is handed out exactly once.
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
    /// If the WAL is degraded and the degradation is one a discard can end, end it.
    ///
    /// By the time this runs every caller has been told its write is not durable, so making those
    /// bytes durable afterwards would be fail-open: an exhausted deny window's `unsuppress` would
    /// undo a suppression the operator was told still stood. The region is discarded instead. A
    /// torn append does not recover: such a node stays `WalPoisoned` until restarted.
    pub(super) fn recover_wal(&mut self) {
        if !self.wal.is_poisoned() {
            return;
        }
        if !self.wal.is_recoverable() {
            return;
        }
        // Deliberately silent about failing: a log line per attempt would turn one storage fault
        // into an unbounded stream of them.
        if self.wal.discard_undurable().is_ok() {
            // The discard did not un-apply anything: every deletion and suppression applied under
            // the apply-anyway rule is in force in memory with no record behind it.
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

    /// Append `records` in order and fsync once: the one path to durability, whichever lane asks
    /// for it.
    ///
    /// Each record's position is read before its own append, because that is the only moment it
    /// can be read, and the positions come back in input order; a rotation reclaims by them, so a
    /// record that failed to append contributes none. On failure nothing the caller prepared is in
    /// force; ids it reserved stay spent, so a torn append that replays cannot land them on
    /// entities a later command also holds.
    ///
    /// `laps` times the appends against [`WriteStage::WalAppend`] and the sync against
    /// [`WriteStage::WalFsync`] for the callers that measure them, and is `None` for the rest.
    pub(super) fn append_and_sync(
        &mut self,
        records: &[&WalRecord],
        laps: Option<StageMark>,
    ) -> std::result::Result<Vec<u64>, Undurable> {
        let mut positions = Vec::with_capacity(records.len());
        let mut failed = None;
        for (at, record) in records.iter().enumerate() {
            let before = self.wal.position();
            if let Err(error) = self.wal.append(record) {
                failed = Some(Undurable::Append { at, error });
                break;
            }
            positions.push(before);
        }
        let mark = laps.map(|mark| self.health.lap(WriteStage::WalAppend, mark));
        if failed.is_none() {
            if let Err(error) = self.wal.fsync() {
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

    /// Reclaim what the publication just made redundant: after the generation swap and never
    /// before it, since rotation writes its snapshot before any deletion. The reclaim bound is the
    /// buffer's oldest surviving row (`IngestBuffer::oldest_wal_pos`), refusing (`None`) rather
    /// than guessing if any buffered row does not know its own position.
    ///
    /// Two gates: a poisoned WAL cannot be appended to at all, and a node whose overlay has
    /// diverged from its durable WAL must rotate nothing, since writing a snapshot from that
    /// overlay would make a 500'd, never-acked deny permanent. Nothing here is fatal.
    pub(super) fn rotate_wal(&mut self) {
        if self.wal.is_poisoned() {
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
        // A stepped-down node reclaims nothing: its WAL members are the only recovery material
        // for whatever the step-down shadowed.
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
            // Nothing buffered: every ingest row has geometry, so the whole durable prefix is
            // reclaimable.
            None => self.wal.position(),
            Some(Some(oldest)) => oldest,
            // A buffered row of unknown position pins the log: fail-safe by construction, since the
            // sequence grows visibly rather than a record vanishing.
            Some(None) => 0,
        };
        // The oldest artifact publication pins the log too: a membership has no home outside the
        // WAL, so reclaiming a member holding one destroys the only copy. Fail-closed: a log that
        // grows is noticed where a membership that vanishes is not.
        let reclaim_below = match self.live.artifacts_oldest_wal_pos() {
            Some(oldest) => reclaim_below.min(oldest),
            None => reclaim_below,
        };

        let snapshot = generation.overlay.snapshot();
        match self.wal.rotate(&snapshot, reclaim_below) {
            Ok(deleted) => {
                // Post-rotation position, so the next growth check counts only appends made
                // after the snapshot this rotation just wrote.
                self.wal_position_at_last_rotation = self.wal.position();
                if !deleted.is_empty() {
                    // The idempotency index follows the log it caches, so entries whose records
                    // lay in the members just deleted go now: otherwise this process would answer
                    // a batch id as a replay that the same node would call unknown after a restart.
                    let forgotten = self.live.forget_batches_below(self.wal.retained_from());
                    tracing::info!(
                        members = ?self.wal.members(),
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

    /// Rotate at the tick when the log has grown and no flush publication is coming to do it: the
    /// deny-only regime's rotation. Without this, a node that took denies without ever flushing
    /// would seal nothing, snapshot nothing and reclaim nothing: an unbounded log replayed in full
    /// at every restart.
    ///
    /// Gated on growth, so an idle node rotates nothing: a rotation writes an O(overlay) snapshot
    /// and a new member, which would be churn for no reclaim on a quiet deployment.
    pub(super) fn rotate_if_grown(&mut self) {
        if self.wal.position() == self.wal_position_at_last_rotation {
            return;
        }
        self.rotate_wal();
    }

    /// Read the WAL's size and its rotation bound into [`ExecutorHealth::wal_gauge`], at most once
    /// per tick period. Rotates nothing, compares nothing against a limit, returns no decision.
    ///
    /// The cost is O(members): under a `growth` or `fill` pin a member accumulates per rotation for
    /// as long as the fold that would release it is refused, so the walk gets dearer as the problem
    /// gets worse. The rate limit below bounds it to once per `flush_max_age_secs`.
    pub(super) fn sample_wal_gauge(&mut self) {
        let period = std::time::Duration::from_secs(self.deps.flush_max_age_secs);
        if let Some(last) = self.last_wal_sample {
            if last.elapsed() < period {
                return;
            }
        }
        // Before the walk, so a slow walk shortens the next interval rather than pushing it out.
        self.last_wal_sample = Some(std::time::Instant::now());
        self.wal_samples += 1;
        let position = self.wal.position();
        let pin = self.live.with_artifacts(|store| store.wal_pin());
        self.health.record_wal_gauge(WalGauge {
            members: self.wal.member_count(),
            bytes: self.wal.disc_bytes(),
            position,
            pin,
            pin_span_bytes: pin.map_or(0, |(_, pos)| position.saturating_sub(pos)),
            samples: self.wal_samples,
        });
    }

    /// Mirror the WAL's own poison flag into the posture, in both directions.
    ///
    /// Asked of the WAL rather than remembered from the last error this loop happened to see, so
    /// the posture cannot drift from the thing it describes.
    pub(super) fn observe_wal(&self) {
        self.health.mirror_wal(self.wal.is_poisoned());
    }
}
