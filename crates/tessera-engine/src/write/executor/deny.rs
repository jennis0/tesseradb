use super::*;

/// The most entries one deny window may hold, and the most changes `/control/changes` enqueues
/// before it collects. Bounds the drain so it terminates under sustained deny arrival. Raising it
/// reduces fsyncs and overlay clones at the cost of larger pending-receipt bursts.
pub const DENY_WINDOW_MAX_ENTRIES: usize = 1_000;

/// How many deny windows may pass before the overlay publishes regardless of whether the drain has
/// closed. A liveness floor: without it the newest manifest could trail live state indefinitely
/// under sustained deny arrival. Bounds the side-manifest lag to at most 64,000 dispositions,
/// already durable in the WAL and recovered by any restart.
pub(super) const OVERLAY_PUBLICATION_MAX_WINDOWS: u64 = 64;

/// How long the executor waits before each re-attempt at making a deny window durable, and
/// therefore how many attempts there are: the first sync, plus one per entry here. The deny lane
/// is FIFO on a single thread, so this delay is paid by every deny queued behind a failing window.
/// Non-zero because an immediate retry cannot help a short-lived `ENOSPC`.
pub(super) const DENY_DURABILITY_BACKOFF: [std::time::Duration; 2] = [
    std::time::Duration::from_millis(50),
    std::time::Duration::from_millis(200),
];

/// How many durability attempts one deny window gets in total: the original sync plus one per
/// [`DENY_DURABILITY_BACKOFF`] entry.
///
/// Public because a test observing the exhausted path must arm exactly this many failures.
pub const DENY_DURABILITY_ATTEMPTS: usize = DENY_DURABILITY_BACKOFF.len() + 1;

/// One deny in an open window: its record, and everything needed to apply it and answer its caller.
///
/// `record` is built at the drain rather than at the append so the window is a list of things that
/// are ready to be written: the append loop does no work that can be got wrong per entry.
pub(super) struct DenyEntry {
    pub(super) record: WalRecord,
    pub(super) entity: EntityId,
    pub(super) op: ChangeOp,
    /// The waiter, or `None` for a cascaded deletion (`Executor::cascade_dependents`), which has no
    /// caller to answer but is otherwise an ordinary entry: its own WAL record, applied in the same
    /// window, retired at the same fold.
    pub(super) reply: Option<Reply<()>>,
}

impl Executor {
    /// The deny window: gather the queued denies into one committable unit and commit it. Returns
    /// whether anything was found, which is what keeps [`Executor::run`] draining before it blocks.
    /// Amortises an item-at-a-time path's per-entry fsync and full [`Overlay`] clone (which shrinks
    /// only at a fold, so an N-item revocation would otherwise copy Θ(N²) entries).
    ///
    /// The window closes when the queue is observed empty or [`DENY_WINDOW_MAX_ENTRIES`] entries
    /// are reached, checked inside the drain since every entry pulled is one a concurrent submitter
    /// can replace. No linger: the deny lane stays unbounded and drained to empty before any work.
    pub(super) fn run_deny_pass(&mut self) -> bool {
        let mut entries: Vec<DenyEntry> = Vec::new();

        while entries.len() < DENY_WINDOW_MAX_ENTRIES {
            let Ok(command) = self.queues.deny.try_recv() else {
                break;
            };
            let Command::Change { entity, op, reply } = command else {
                // Only a `Change` rides the deny queue; this arm applies immediately, so the
                // window gathered so far is committed first to keep append order equal to apply
                // order.
                if !entries.is_empty() {
                    self.commit_denies(std::mem::take(&mut entries));
                }
                self.execute(command);
                return true;
            };
            entries.push(DenyEntry {
                record: WalRecord::ChangeByEntity {
                    entity_id: entity,
                    op,
                },
                entity,
                op,
                reply: Some(reply),
            });
        }

        if entries.is_empty() {
            return false;
        }
        self.cascade_dependents(&mut entries);
        self.commit_denies(entries);
        true
    }

    /// Add a deletion for every artifact that depends on one this window deletes.
    ///
    /// A cascaded deletion is an ordinary entry: its own `ChangeByEntity` record in the same
    /// append, applied to the same overlay clone, retired at the same fold. Added before the
    /// append, so a restart rebuilds the same cascade from the log rather than re-deriving it. Only
    /// `Delete` cascades: a suppressed dependent is withheld by the serving predicate instead.
    pub(super) fn cascade_dependents(&mut self, entries: &mut Vec<DenyEntry>) {
        let deleted: Vec<EntityId> = entries
            .iter()
            .filter(|e| matches!(e.op, ChangeOp::Delete))
            .map(|e| e.entity)
            .collect();
        if deleted.is_empty() {
            return;
        }
        let cascade = self
            .live
            .with_artifacts(|store| store.cascade_from(&deleted));
        for entity in cascade {
            entries.push(DenyEntry {
                record: WalRecord::ChangeByEntity {
                    entity_id: entity,
                    op: ChangeOp::Delete,
                },
                entity,
                op: ChangeOp::Delete,
                reply: None,
            });
        }
    }

    /// `append × k → one fsync → apply → one swap → ack × k`, with the apply-anyway exception for
    /// deny ops folded per entry. Append order is entries order is apply order, so a `suppress D`
    /// and a later `unsuppress D` in the same window resolve as they would have as two commands.
    ///
    /// On an unrepaired append or fsync failure, every [`ChangeOp::Delete`] and
    /// [`ChangeOp::Suppress`] in the window is applied anyway, hiding the items immediately, and
    /// every waiter still gets an error; every [`ChangeOp::Unsuppress`] applies nothing. Replay
    /// discards every record the window appended, so the applied `Suppress` comes back unhidden on
    /// restart: durability was owed and not reached, and the caller must retry. The item stays
    /// hidden in memory until then, and the node stops claiming readiness.
    pub(super) fn commit_denies(&mut self, entries: Vec<DenyEntry>) {
        // One fsync for the whole window. Every entry is durable when it returns, or none is.
        let records: Vec<&WalRecord> = entries.iter().map(|entry| &entry.record).collect();
        let failed_at = match self.append_and_sync(&records, None) {
            Ok(_) => None,
            Err(Undurable::Append { at, error }) => Some((at, error)),
            // A sync is retried where a torn append is not; the first entry is blamed for an
            // exhausted retry, since no one of them failed.
            Err(Undurable::Fsync(error)) => self
                .retry_deny_durability(&entries, error)
                .err()
                .map(|e| (0, e)),
        };
        self.observe_wal();

        if let Some((index, error)) = failed_at {
            let applied: Vec<(EntityId, ChangeOp)> = entries
                .iter()
                .filter(|e| matches!(e.op, ChangeOp::Delete | ChangeOp::Suppress))
                .map(|e| (e.entity, e.op))
                .collect();
            if !applied.is_empty() {
                // Deliberately does not mark the overlay dirty: publishing these would make a
                // never-acked deny permanent, since no durable record backs them.
                self.apply_changes(applied);
            }
            let mut blame = Blame::new(index, error);
            for (i, entry) in entries.into_iter().enumerate() {
                let e = blame.at(i);
                if let Some(reply) = &entry.reply {
                    reply.fail(ExecError::Wal(e));
                }
            }
            return;
        }

        // Durable, not yet in force. See `pause_point`.
        self.pause_point(PauseSiteArg::AfterFsync);

        let applied: Vec<(EntityId, ChangeOp)> = entries.iter().map(|e| (e.entity, e.op)).collect();

        self.apply_changes(applied);
        self.side_manifests.behind_live = true;
        self.side_manifests.windows_since_publication += 1;
        if self.side_manifests.windows_since_publication >= OVERLAY_PUBLICATION_MAX_WINDOWS {
            self.publish_overlay_state();
        }

        // A death partway through this loop leaves some waiters unacked; each gets
        // `SubmitError::ReceiptLost` → 500, never `ExecutorDead` → 503, since its change is
        // durably in force.
        for entry in entries {
            if let Some(reply) = &entry.reply {
                reply.ack(());
            }
        }
    }

    /// A deny window's sync failed. Re-write its records and sync again, up to
    /// [`DENY_DURABILITY_ATTEMPTS`] times in total, and report whether durability was reached.
    ///
    /// The deny lane retries and the ingest lane does not: a deny window's failure applies its
    /// deletions and suppressions anyway, so a retry here can still change what the live node shows
    /// before a restart un-hides them. A bare second `fsync` is not a retry on Linux, since the
    /// kernel may report a writeback error exactly once; `Wal::retry_durability` rewinds to the
    /// last durable offset and re-writes the records instead. Runs before the window is applied,
    /// since entries order must stay apply order for a `suppress D` followed by an `unsuppress D`.
    pub(super) fn retry_deny_durability(
        &mut self,
        entries: &[DenyEntry],
        first: WalError,
    ) -> std::result::Result<(), WalError> {
        let records: Vec<WalRecord> = entries.iter().map(|e| e.record.clone()).collect();
        let mut last = first;
        for delay in DENY_DURABILITY_BACKOFF {
            std::thread::sleep(delay);
            match self.log.wal.retry_durability(&records) {
                Ok(_) => return Ok(()),
                Err(e) => last = e,
            }
        }
        Err(last)
    }

    /// Clone the overlay once, apply every change in the window, publish once. [`Overlay`] never
    /// shrinks except at a fold, so the clone is O(overlay depth). Changes are applied in the
    /// window's entries order, the deny lane's FIFO order, so a `suppress` and a later `unsuppress`
    /// of the same item resolve as two separate commands would have.
    ///
    /// Pins are never invalidated by this: a pin fixes `(prefix, segments_version)`, and this bumps
    /// `overlay_version` instead, so a suppression applies to a pinned request the moment accepted.
    pub(super) fn apply_changes(&self, changes: Vec<(EntityId, ChangeOp)>) {
        let started = std::time::Instant::now();
        let generation = self.generation.load_full();
        let mut overlay: Overlay = (*generation.overlay).clone();
        let mut newly_denied: Vec<EntityId> = Vec::new();
        let mut deleted: Vec<EntityId> = Vec::new();
        let mut unsuppressed = false;
        for (entity, op) in changes {
            match op {
                ChangeOp::Delete => {
                    newly_denied.push(entity);
                    deleted.push(entity);
                }
                ChangeOp::Suppress => newly_denied.push(entity),
                ChangeOp::Unsuppress => unsuppressed = true,
            }
            overlay.apply(entity, op);
        }

        // `overlay_soft_limit` gauges `deleted ∪ suppressed`, since a suppression never retires and
        // a fold dispatched on the union would rewrite the corpus to retire nothing.
        //
        // Edge-triggered: the depth never decreases, so a level-triggered check would emit this
        // WARN on every subsequent deny, forever, with no path back.
        let depth = overlay.len();
        let limit = self.health.overlay_soft_limit();
        if self.health.note_overlay_depth(depth) {
            tracing::warn!(
                overlay_depth = depth,
                overlay_soft_limit = limit,
                "ALARM: the overlay has crossed its configured soft limit. A compaction fold \
                 retires the executed deletions, but nothing schedules one automatically; watch \
                 overlay.depth on /control/status"
            );
        }

        // A deleted row leaves the buffer here: `plan_flush` never consumes a deleted row, so
        // nothing else would ever remove it, and a `delete` issued before the item's first flush
        // would otherwise pin the WAL forever.
        //
        // The clone is paid only when a buffered row is actually dropped. Deleting an entity that
        // already has geometry, the ordinary case, costs one hash lookup and no clone.
        let buffer = if !generation.buffer.holds_any(&deleted) {
            Arc::clone(&generation.buffer)
        } else {
            let mut buffer = (*generation.buffer).clone();
            for entity in deleted {
                buffer.remove(entity);
            }
            self.health
                .buffered_items
                .store(buffer.len(), Ordering::SeqCst);
            Arc::new(buffer)
        };

        // A window of deletes and suppressions only grows the mask, so their rows are added. An
        // unsuppress derives it afresh: subtracting a row would re-expose an entity that is still
        // deleted.
        let overlay_version = generation.overlay_version + 1;
        let next = if unsuppressed {
            generation.with(|g| {
                g.overlay_version = overlay_version;
                g.overlay = Arc::new(overlay);
                g.buffer = buffer;
            })
        } else {
            generation.with_denies(Arc::new(overlay), &newly_denied, |g| {
                g.overlay_version = overlay_version;
                g.buffer = buffer;
            })
        };
        self.publish(next, started)
    }
}
