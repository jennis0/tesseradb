use super::*;

/// The most changes one deny window gathers before it closes and commits; a single request larger
/// than this is committed whole. Raising it reduces
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

/// One request in an open deny window, with everything needed to apply it and answer its caller.
pub(super) struct DenyEntry {
    /// In request order; one record carries them all.
    pub(super) changes: Vec<(Target, ChangeOp)>,
    /// `None` for the deletions a window's deletions cascade to, which have no caller to answer
    /// but are otherwise an ordinary entry.
    pub(super) reply: Option<Reply<crate::resolver::Refused>>,
    /// The request's rows that named nothing where it applied, which its answer lists.
    pub(super) refused: crate::resolver::Refused,
}

impl DenyEntry {
    fn record(&self) -> WalRecord {
        WalRecord::ChangeBatch {
            changes: self.changes.clone(),
        }
    }
}

impl Executor {
    /// Gather the queued deny requests into one committable unit and commit it, closing at an
    /// empty queue or once [`DENY_WINDOW_MAX_ENTRIES`] changes are held. Returns whether anything
    /// was found.
    pub(super) fn run_deny_pass(&mut self) -> bool {
        let mut entries: Vec<DenyEntry> = Vec::new();
        let mut held = 0usize;

        while held < DENY_WINDOW_MAX_ENTRIES {
            let Ok(command) = self.queues.deny.try_recv() else {
                break;
            };
            let Command::Changes {
                mut names,
                ops,
                strict,
                reply,
            } = command
            else {
                // Any other command applies immediately, so the window gathered so far is
                // committed first to keep append order equal to apply order.
                if !entries.is_empty() {
                    self.commit_denies(std::mem::take(&mut entries));
                }
                self.execute(command);
                return true;
            };
            let (changes, refused) = match self.changes_now(&mut names, &ops, strict) {
                Ok((changes, refused)) if changes.is_empty() => {
                    reply.ack(refused);
                    continue;
                }
                Ok(answer) => answer,
                Err(e) => {
                    reply.fail(e);
                    continue;
                }
            };
            held += changes.len();
            entries.push(DenyEntry {
                changes,
                reply: Some(reply),
                refused,
            });
        }

        if entries.is_empty() {
            return false;
        }
        self.cascade_dependents(&mut entries);
        self.commit_denies(entries);
        true
    }

    /// A request's changes as they apply now, row `i` of `names` taking `ops[i]`, and the rows
    /// that name nothing now: on this thread, which is the only one that moves an item, so
    /// nothing moves one between this and the apply ([`crate::Names::now`]). A row whose item an
    /// edit has moved also changes the entity it named, which the log then names beside the one
    /// the item holds. `Err` where the names cannot be read, or where `strict` and a row is
    /// refused.
    pub(super) fn changes_now(
        &self,
        names: &mut crate::resolver::Names,
        ops: &[ChangeOp],
        strict: bool,
    ) -> std::result::Result<(Vec<(Target, ChangeOp)>, crate::resolver::Refused), ExecError> {
        let generation = self.generation.load_full();
        let scope = names.scope().clone();
        let resolver = crate::resolver::Resolver::new(
            &generation,
            &scope,
            &self.deps.identity_key,
            &self.live,
        );
        let moved = names.now(&resolver).map_err(|e| ExecError::Store {
            detail: e.to_string(),
        })?;
        let refused = names.refused();
        if let Some(&(row, reason)) = refused.first().filter(|_| strict) {
            return Err(ExecError::NamesRefused { row, reason });
        }
        let mut changes: Vec<(Target, ChangeOp)> = ops
            .iter()
            .enumerate()
            .filter_map(|(row, op)| Some((names.verdict(row).ok()?, *op)))
            .collect();
        for crate::resolver::Moved { row, was } in moved {
            let (Some(Target::Item(shard, old)), Ok(Target::Item(at, now))) =
                (was, names.verdict(row))
            else {
                continue;
            };
            let Some(held) = generation
                .shard(shard)
                .filter(|_| at == shard && now != old)
            else {
                continue;
            };
            let numbers =
                crate::edited::numbers_of(held, &[old, now]).map_err(|e| ExecError::Store {
                    detail: e.to_string(),
                })?;
            if numbers[0] == numbers[1] && held.overlay.is_deleted(old) {
                changes.push((Target::Item(shard, old), ops[row]));
            }
        }
        Ok((changes, refused))
    }

    /// Add one entry deleting every artifact that depends on one this window deletes. Only
    /// `Delete` cascades: a suppressed dependent is withheld by the serving predicate instead.
    pub(super) fn cascade_dependents(&mut self, entries: &mut Vec<DenyEntry>) {
        let deleted: Vec<Target> = entries
            .iter()
            .flat_map(|e| e.changes.iter())
            .filter(|(_, op)| matches!(op, ChangeOp::Delete))
            .map(|(target, _)| *target)
            .collect();
        let cascade = self.live.dependents_of(&deleted);
        if cascade.is_empty() {
            return;
        }
        entries.push(DenyEntry {
            changes: cascade
                .into_iter()
                .map(|target| (target, ChangeOp::Delete))
                .collect(),
            reply: None,
            refused: Vec::new(),
        });
    }

    /// One fsync for the whole window, then apply, then one swap, then ack. A deleted artifact's
    /// key is free from the acknowledgement. On a failed append or fsync, every
    /// [`ChangeOp::Delete`] and [`ChangeOp::Suppress`] in the window is applied anyway and every
    /// waiter gets an error; [`ChangeOp::Unsuppress`] applies nothing, and no key is freed, since
    /// a restart without the record brings the artifact back under its key. The overlay is not
    /// marked behind-live for these: they have no durable record behind them, so publishing them
    /// into a manifest would make a deny the caller was told failed permanent.
    pub(super) fn commit_denies(&mut self, entries: Vec<DenyEntry>) {
        let owned: Vec<WalRecord> = entries.iter().map(DenyEntry::record).collect();
        let records: Vec<&WalRecord> = owned.iter().collect();
        let failed_at = match self.append_and_sync(&records, None) {
            Ok(_) => None,
            Err(Undurable::Append { at, error }) => Some((at, error)),
            // A sync is retried where a torn append is not; the first entry is blamed since no
            // one of them failed.
            Err(Undurable::Fsync(error)) => self
                .retry_deny_durability(&owned, error)
                .err()
                .map(|e| (0, e)),
        };
        self.observe_wal();

        if let Some((index, error)) = failed_at {
            let applied: Vec<(Target, ChangeOp)> = entries
                .iter()
                .flat_map(|e| e.changes.iter().copied())
                .filter(|(_, op)| matches!(op, ChangeOp::Delete | ChangeOp::Suppress))
                .collect();
            if !applied.is_empty() {
                self.apply_changes(applied);
            }
            let mut blame = Blame::new(index, error);
            for (i, entry) in entries.into_iter().enumerate() {
                let e = blame.at(i);
                if let Some(reply) = entry.reply {
                    reply.fail(ExecError::Wal(e));
                }
            }
            return;
        }

        // Durable, not yet in force. See `pause_point`.
        self.pause_point(PauseSiteArg::AfterFsync);

        let applied: Vec<(Target, ChangeOp)> = entries
            .iter()
            .flat_map(|e| e.changes.iter().copied())
            .collect();
        let deleted_artifacts: Vec<(LayerSpace, EntityId)> = applied
            .iter()
            .filter_map(|change| match change {
                (Target::Artifact(space, entity), ChangeOp::Delete) => Some((*space, *entity)),
                _ => None,
            })
            .collect();

        self.apply_changes(applied);
        self.live.release_artifact_keys(&deleted_artifacts);
        self.side_manifests.behind_live = true;
        self.side_manifests.windows_since_publication += 1;
        if self.side_manifests.windows_since_publication >= OVERLAY_PUBLICATION_MAX_WINDOWS {
            self.publish_overlay_state();
        }

        // A death partway leaves some waiters unacked; each gets a 500, not a 503, since durable.
        for entry in entries {
            if let Some(reply) = entry.reply {
                reply.ack(entry.refused);
            }
        }
        // After the acknowledgement and on the figures' own thread: the next request finds the
        // denied rows' labels brought forward rather than reading them.
        self.deps.figures.refresh_denied(
            &self.generation.load(),
            &self.deps.artifact_projections,
            &self.live.registered_layers(),
        );
    }

    /// A deny window's sync failed: rewrite its records and sync again, up to
    /// [`DENY_DURABILITY_ATTEMPTS`] times. The deny lane retries where ingest does not, since a
    /// deny already applies its deletions anyway and a retry can still land before a restart
    /// un-hides them. A bare second `fsync` is not a retry on Linux: the kernel may report a
    /// writeback error once, so this rewinds to the last durable offset and rewrites instead.
    pub(super) fn retry_deny_durability(
        &mut self,
        records: &[WalRecord],
        first: WalError,
    ) -> std::result::Result<(), WalError> {
        let mut last = first;
        for delay in DENY_DURABILITY_BACKOFF {
            std::thread::sleep(delay);
            match self.log.wal.retry_durability(records) {
                Ok(_) => return Ok(()),
                Err(e) => last = e,
            }
        }
        Err(last)
    }

    /// Clone each overlay the window touches once, apply every change in the window in order to
    /// the overlay its target names, publish once over every shard touched. Pins are never
    /// invalidated by this: a pin fixes `(prefix, segments_version)`, and this bumps the overlay
    /// versions instead, so a suppression applies to a pinned request the moment accepted.
    pub(super) fn apply_changes(&self, changes: Vec<(Target, ChangeOp)>) {
        let started = std::time::Instant::now();
        let generation = self.generation.load_full();
        let mut shards: std::collections::BTreeMap<ShardId, ShardChanges> = Default::default();
        let mut spaces = (*generation.layer_spaces).clone();
        let mut touched: std::collections::BTreeSet<LayerSpace> = Default::default();
        for (target, op) in changes {
            let (id, entity) = match target {
                Target::Item(shard, entity) => (shard, entity),
                // A change resolved before its layer was dropped names a space nothing holds.
                Target::Artifact(space, _) if !self.live.holds_space(space) => continue,
                Target::Artifact(space, entity) => {
                    let parts = spaces.entry(space).or_default();
                    if touched.insert(space) {
                        parts.overlay_version += 1;
                    }
                    Arc::make_mut(&mut parts.overlay).apply(entity, op);
                    continue;
                }
            };
            let changed = shards.entry(id).or_insert_with(|| {
                let shard = generation.shard_of(id);
                ShardChanges {
                    overlay: Arc::clone(&shard.overlay),
                    buffer: Arc::clone(&shard.buffer),
                    newly_denied: Vec::new(),
                    deleted: Vec::new(),
                    unsuppressed: false,
                }
            });
            match op {
                ChangeOp::Delete => {
                    changed.newly_denied.push(entity);
                    changed.deleted.push(entity);
                }
                ChangeOp::Suppress => changed.newly_denied.push(entity),
                ChangeOp::Unsuppress => changed.unsuppressed = true,
            }
            Arc::make_mut(&mut changed.overlay).apply(entity, op);
        }
        let layer_spaces = Arc::new(spaces);
        let deny_epoch = generation.deny_epoch + 1;
        if shards.is_empty() {
            let next = generation.with(|g| {
                g.deny_epoch = deny_epoch;
                g.layer_spaces = layer_spaces;
            });
            self.publish(next, started);
            return;
        }

        // Gauges `deleted ∪ suppressed` over every shard. Edge-triggered, or this WARN would fire
        // on every subsequent deny once the limit is crossed.
        let depth: usize = generation
            .shards()
            .map(|shard| match shards.get(&shard.id()) {
                Some(changed) => changed.overlay.len(),
                None => shard.overlay.len(),
            })
            .sum();
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

        // A deleted entity's rows leave its shard's buffer here, or they would pin the WAL:
        // `plan_flush` never consumes a deleted row. A deleted entity names nothing, so its live
        // unique entries go with its rows. Its edited-item pair stays until the fold that removes
        // it, which reads the pair to tell an entity an edit made from an item's number.
        let mut unique_live: Option<crate::unique::UniqueLive> = None;
        for (id, changed) in shards.iter_mut() {
            if !changed.buffer.holds_any(&changed.deleted) {
                continue;
            }
            let buffer = Arc::make_mut(&mut changed.buffer);
            for entity in &changed.deleted {
                buffer.remove(*entity);
            }
            unique_live
                .get_or_insert_with(|| (*generation.unique_live).clone())
                .remove_entities(
                    *id,
                    &changed
                        .deleted
                        .iter()
                        .filter_map(|e| u32::try_from(e.raw()).ok())
                        .collect(),
                );
        }

        // A window of deletes and suppressions only grows a shard's mask; an unsuppress re-derives
        // it, since subtracting a row would re-expose a still-deleted entity.
        let swaps = shards
            .iter()
            .map(|(id, changed)| {
                let buffer = crate::BufferSwap {
                    buffer: Arc::clone(&changed.buffer),
                    inserted: &[],
                };
                let swap = match changed.unsuppressed {
                    true => crate::ShardSwap::Buffer(buffer),
                    false => crate::ShardSwap::Denies(crate::DenySwap {
                        overlay: Arc::clone(&changed.overlay),
                        newly_denied: &changed.newly_denied,
                        buffer,
                    }),
                };
                (*id, swap)
            })
            .collect();
        let next = generation.publish(swaps, |g, parts| {
            g.deny_epoch = deny_epoch;
            g.layer_spaces = layer_spaces;
            if let Some(unique_live) = unique_live {
                g.unique_live = Arc::new(unique_live);
            }
            for (id, changed) in &shards {
                let s = parts.of(*id);
                s.overlay_version += 1;
                if changed.unsuppressed {
                    s.overlay = Arc::clone(&changed.overlay);
                }
            }
        });
        self.health
            .buffered_items
            .store(next.buffered(), Ordering::SeqCst);
        self.publish(next, started)
    }
}

/// One shard's part of a deny window: its overlay with the window's changes applied, its buffer
/// without the deleted entities' rows, and what the changes deny.
struct ShardChanges {
    overlay: Arc<Overlay>,
    buffer: Arc<IngestBuffer>,
    newly_denied: Vec<EntityId>,
    deleted: Vec<EntityId>,
    unsuppressed: bool,
}
