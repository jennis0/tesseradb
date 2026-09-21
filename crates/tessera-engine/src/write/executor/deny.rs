use super::*;

/// The most entries one deny window may hold before it closes and commits. Raising it reduces
/// fsyncs and overlay clones at the cost of larger pending-receipt bursts.
pub const DENY_WINDOW_MAX_ENTRIES: usize = 1_000;

/// How many deny windows may pass before the overlay publishes regardless of drain state, so the
/// side-manifest lag stays bounded (at most 64,000 dispositions, already durable in the WAL).
pub(super) const OVERLAY_PUBLICATION_MAX_WINDOWS: u64 = 64;

/// Delay before each re-attempt at making a deny window durable, non-zero since an immediate retry
/// cannot help a short-lived `ENOSPC`. Paid by every deny queued behind a failing window.
pub(super) const DENY_DURABILITY_BACKOFF: [std::time::Duration; 2] = [
    std::time::Duration::from_millis(50),
    std::time::Duration::from_millis(200),
];

/// Total durability attempts for one window: the original sync plus one per backoff entry. Public
/// so a test can arm exactly this many failures.
pub const DENY_DURABILITY_ATTEMPTS: usize = DENY_DURABILITY_BACKOFF.len() + 1;

/// One deny in an open window, with everything needed to apply it and answer its caller.
pub(super) struct DenyEntry {
    pub(super) record: WalRecord,
    pub(super) entity: EntityId,
    pub(super) op: ChangeOp,
    /// `None` for a cascaded deletion, which has no caller to answer but is otherwise an ordinary
    /// entry.
    pub(super) reply: Option<Reply<()>>,
}

impl Executor {
    /// Gather the queued denies into one committable unit and commit it, closing at an empty queue
    /// or [`DENY_WINDOW_MAX_ENTRIES`]. Returns whether anything was found.
    pub(super) fn run_deny_pass(&mut self) -> bool {
        let mut entries: Vec<DenyEntry> = Vec::new();

        while entries.len() < DENY_WINDOW_MAX_ENTRIES {
            let Ok(command) = self.queues.deny.try_recv() else {
                break;
            };
            let Command::Change { entity, op, reply } = command else {
                // Any other command applies immediately, so the window gathered so far is
                // committed first to keep append order equal to apply order.
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

    /// Add a deletion for every artifact that depends on one this window deletes. Only `Delete`
    /// cascades: a suppressed dependent is withheld by the serving predicate instead.
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

    /// One fsync for the whole window, then apply, then one swap, then ack. On a failed append or
    /// fsync, every [`ChangeOp::Delete`] and [`ChangeOp::Suppress`] in the window is applied anyway
    /// and every waiter gets an error; [`ChangeOp::Unsuppress`] applies nothing. The overlay is not
    /// marked behind-live for these: they have no durable record behind them, so publishing them
    /// into a manifest would make a deny the caller was told failed permanent.
    pub(super) fn commit_denies(&mut self, entries: Vec<DenyEntry>) {
        let records: Vec<&WalRecord> = entries.iter().map(|entry| &entry.record).collect();
        let failed_at = match self.append_and_sync(&records, None) {
            Ok(_) => None,
            Err(Undurable::Append { at, error }) => Some((at, error)),
            // A sync is retried where a torn append is not; the first entry is blamed since no
            // one of them failed.
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

        // A death partway leaves some waiters unacked; each gets a 500, not a 503, since durable.
        for entry in entries {
            if let Some(reply) = &entry.reply {
                reply.ack(());
            }
        }
    }

    /// A deny window's sync failed: rewrite its records and sync again, up to
    /// [`DENY_DURABILITY_ATTEMPTS`] times. The deny lane retries where ingest does not, since a
    /// deny already applies its deletions anyway and a retry can still land before a restart
    /// un-hides them. A bare second `fsync` is not a retry on Linux: the kernel may report a
    /// writeback error once, so this rewinds to the last durable offset and rewrites instead.
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

    /// Clone the overlay once, apply every change in the window in order, publish once. Pins are
    /// never invalidated by this: a pin fixes `(prefix, segments_version)`, and this bumps
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

        // Gauges `deleted ∪ suppressed`. Edge-triggered, or this WARN would fire on every
        // subsequent deny once the limit is crossed.
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

        // A deleted entity's rows leave the buffer here, or they would pin the WAL: `plan_flush`
        // never consumes a deleted row.
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

        // A window of deletes and suppressions only grows the mask; an unsuppress re-derives it,
        // since subtracting a row would re-expose a still-deleted entity.
        let overlay_version = generation.overlay_version + 1;
        let next = if unsuppressed {
            generation.with_buffer(buffer, &[], |g| {
                g.overlay_version = overlay_version;
                g.overlay = Arc::new(overlay);
            })
        } else {
            generation.with_denies(Arc::new(overlay), &newly_denied, buffer, |g| {
                g.overlay_version = overlay_version;
            })
        };
        self.publish(next, started)
    }
}
