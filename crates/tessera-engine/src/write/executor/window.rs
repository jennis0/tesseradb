use super::*;

/// What `/control/ingest`'s batch id means to this executor: durably accepted, held in the open
/// commit window, or never seen. Computed by [`BatchState::of`] on the executor thread only.
pub(super) enum BatchState {
    /// Durably accepted: the WAL record is fsynced, the rows are applied and the ids are recorded.
    /// Same bytes replays these ids; different bytes is a `409`.
    Accepted {
        body_hash: [u8; 32],
        entity_ids: Vec<EntityId>,
    },
    /// Held in the open commit window, not yet acknowledged. Allocation happens at the close, so a
    /// byte-identical retry joins the queue of waiters the entry will ack.
    ///
    /// `window_seq` is the window the entry was found in; there is exactly one open window.
    Held {
        window_seq: u64,
        body_hash: [u8; 32],
    },
    /// Never seen. A new entry.
    ///
    /// Also what an accepted batch regresses to once WAL rotation reclaims its member: a retry with
    /// an `external_id` then 409s on the duplicate check, but one with none is re-ingested as a new
    /// entity. The horizon is about one flush; a client needing longer carries its own id column.
    Unknown,
}

impl BatchState {
    /// Look `batch_id` up: the durable index, then the open window, then unknown. The two sets are
    /// disjoint: a batch id enters `accepted_batches` only at `close_window`. Durable is checked
    /// first because it is the half that survives a restart.
    pub(super) fn of<W>(live: &LiveState, window: &CommitWindow<W>, batch_id: &str) -> BatchState {
        if let Some((body_hash, entity_ids)) = live.accepted_batch(batch_id) {
            return BatchState::Accepted {
                body_hash,
                entity_ids,
            };
        }
        match window.held(batch_id) {
            Some((window_seq, body_hash)) => BatchState::Held {
                window_seq,
                body_hash,
            },
            None => BatchState::Unknown,
        }
    }
}

/// What [`Executor::admit_ingest`] did with a submission, as far as the drain loop needs to know.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum Admission {
    /// It is in the window (or its waiters).
    Admitted,
    /// It was answered outright (a replay, a join or a 409) and nothing was added to the window.
    Answered,
    /// A conflicting external id forced the open window to close. The pass must yield to
    /// `Executor::run`'s deny drain.
    YieldedAfterClose,
}

impl Executor {
    /// Drains the work queue into one commit window and closes it. Returns whether anything was
    /// done, so [`Executor::run`] re-drains the deny lane instead of blocking.
    ///
    /// A window closes when it reaches `commit_window_max_rows`, when the queue is empty, or when an
    /// entry names an external id the window already holds. The row bound is checked inside the
    /// drain, because under load the queue never empties. Every close returns to the run loop, which
    /// is what bounds a deny's wait to the window in front of it: do not keep draining after a close.
    pub(super) fn run_work_pass(&mut self) -> bool {
        let max_rows = self.health.commit_window_max_rows();
        let mut window: CommitWindow<Reply<Ingested>> = CommitWindow::new(self.next_window_seq());
        let mut did_work = false;

        loop {
            if window.rows() >= max_rows {
                self.close_window(window);
                return true;
            }
            let Ok(work) = self.queues.work.try_recv() else {
                break;
            };
            let command = match work {
                ExecutorWork::Lifecycle(command) => command,
                ExecutorWork::PublishGeometry {
                    publication,
                    respond,
                } => {
                    // A publication swaps the whole generation, so the open window closes first.
                    if !window.is_empty() {
                        window = self.close_and_reopen(window);
                    }
                    let _ = respond.send(self.publish_geometry(publication));
                    self.health.note_work_refused();
                    did_work = true;
                    continue;
                }
                #[cfg(feature = "fault-injection")]
                ExecutorWork::ForgetSuggestionIndex {
                    vocabulary,
                    respond,
                } => {
                    if !window.is_empty() {
                        window = self.close_and_reopen(window);
                    }
                    self.forget_suggestion_index(&vocabulary);
                    let _ = respond.send(());
                    did_work = true;
                    continue;
                }
                #[cfg(feature = "fault-injection")]
                ExecutorWork::RebuildSuggestionIndex {
                    vocabulary,
                    respond,
                } => {
                    // The rebuild reads the live minter, which a window holding a minting ingest
                    // has not published yet.
                    if !window.is_empty() {
                        window = self.close_and_reopen(window);
                    }
                    self.rebuild_suggestion_index_now(&vocabulary);
                    let _ = respond.send(());
                    did_work = true;
                    continue;
                }
            };
            let Command::Ingest {
                rows,
                batch_id,
                body_hash,
                artifacts,
                reply,
            } = command
            else {
                // Applies immediately while a window holding earlier ingest is still open, so WAL
                // append order stops equalling submission order: tolerable here since none of
                // these touch the buffer or swap the generation.
                self.execute(command);
                self.health.note_work_refused();
                did_work = true;
                continue;
            };

            let admitted;
            let m = StageMark::now();
            (window, admitted) =
                self.admit_ingest(window, rows, batch_id, body_hash, artifacts, reply);
            self.health.lap(WriteStage::AdmitWindow, m);
            did_work = true;
            if admitted == Admission::YieldedAfterClose {
                break;
            }
        }

        if !window.is_empty() {
            self.close_window(window);
            did_work = true;
        }
        did_work
    }

    /// Close `window` and return its replacement.
    ///
    /// `CommitWindow::new` must stamp the replacement's `opened_at` after `close_window` runs, not
    /// before: stamped first, a replacement would charge its predecessor's whole service to itself,
    /// doubling `record_window_service` and the `retry_after_s` a shed client is told.
    pub(super) fn close_and_reopen(&mut self, window: CommitWindow<Reply<Ingested>>) -> CommitWindow<Reply<Ingested>> {
        self.close_window(window);
        CommitWindow::new(self.next_window_seq())
    }

    pub(super) fn next_window_seq(&mut self) -> u64 {
        self.window_seq += 1;
        self.window_seq
    }

    /// The batch-id state machine, evaluated on the executor.
    ///
    /// Takes the open window by value and hands it back, possibly replaced, so the conflict path
    /// cannot construct the replacement before the close it replaces.
    ///
    /// Lookup order is durable index, then open window, then unknown, evaluated here rather than in
    /// the handler: between a handler check and the enqueue the window can close, so a retry that
    /// saw unknown and then enqueued into a fresh window would have double-allocated.
    pub(super) fn admit_ingest(
        &mut self,
        mut window: CommitWindow<Reply<Ingested>>,
        rows: Vec<UnallocatedRow>,
        batch_id: String,
        body_hash: [u8; 32],
        artifacts: tessera_lifecycle::BatchArtifacts,
        reply: Reply<Ingested>,
    ) -> (CommitWindow<Reply<Ingested>>, Admission) {
        match BatchState::of(&self.live, &window, &batch_id) {
            BatchState::Accepted {
                body_hash: prev_hash,
                entity_ids,
            } => {
                if prev_hash == body_hash {
                    // A replay mints nothing: this batch's keys were created when first accepted.
                    reply.ack(Ingested {
                            entity_ids,
                            minted: 0,
                        },
                    );
                } else {
                    reply.fail(ExecError::BatchConflict { batch_id });
                }
                self.health.note_work_refused();
                (window, Admission::Answered)
            }
            BatchState::Held {
                window_seq,
                body_hash: prev_hash,
            } => {
                debug_assert_eq!(
                    window_seq,
                    window.seq(),
                    "the entry must be joined to the window it was found in"
                );
                if prev_hash == body_hash {
                    let joined = window.join(&batch_id, reply);
                    debug_assert!(joined, "`held` just answered for this batch id");
                } else {
                    // The 409 reaches the retry, not the held original, which is still owed its ack.
                    reply.fail(ExecError::BatchConflict { batch_id });
                }
                self.health.note_work_refused();
                (window, Admission::Answered)
            }
            BatchState::Unknown => {
                let mut admission = Admission::Admitted;
                // Only external ids reach here: a held batch id was answered above.
                if window.holds_external_id_of(&rows) {
                    window = self.close_and_reopen(window);
                    admission = Admission::YieldedAfterClose;
                }
                if let Some(entry) = self.admit(rows, batch_id, body_hash, artifacts, reply) {
                    if window.is_empty() {
                        // Armed at the first entry: an empty window is never closed.
                        self.health.mark_work_started(window.opened_at());
                    }
                    window.push(entry);
                }
                (window, admission)
            }
        }
    }

    /// The external-id admission check, on the one thread that also performs the inserts. `None`
    /// means the caller has already been answered. This check reads state written at apply, which
    /// is why an entry naming an external id the open window holds must close it first.
    ///
    /// The membership column's keys resolve here too, and a bad one refuses the whole batch: one
    /// caller's typo must not refuse another caller's rows in the same window. On an open layer the
    /// same key mints, but its ordinal is not claimed here; see `Executor::mint_records`.
    pub(super) fn admit(
        &mut self,
        rows: Vec<UnallocatedRow>,
        batch_id: String,
        body_hash: [u8; 32],
        artifacts: tessera_lifecycle::BatchArtifacts,
        reply: Reply<Ingested>,
    ) -> Option<WindowEntry<Reply<Ingested>>> {
        // The fail-closed backstop for the widened check-to-apply race, read from the same
        // generation the apply below clones from, so the deleted-holder exemption cannot race
        // its own delete.
        let generation = self.generation.load();
        let mut rows = rows;
        let collisions = self.live.established_collisions(
            &mut rows,
            |e| generation.overlay.is_deleted(e),
            |entity, view| {
                generation.bundle.partitions.values().any(|partition| {
                    partition
                        .views
                        .get(view)
                        .is_some_and(|data| data.row_space.row_of(entity).is_some())
                }) || generation.buffer.contains_in_view(entity, view)
            },
        );
        if collisions == 0 {
            if let Err(detail) = settle_joins(&generation, &mut rows) {
                drop(generation);
                reply.fail(ExecError::JoinRefused { detail });
                self.health.note_work_refused();
                return None;
            }
        }
        drop(generation);
        if collisions > 0 {
            reply.fail(ExecError::DuplicateExternalId { count: collisions },
            );
            self.health.note_work_refused();
            return None;
        }

        let (memberships, edges) = match self.resolve_memberships(&artifacts) {
            Ok(resolved) => resolved,
            Err(detail) => {
                reply.fail(ExecError::LayerRefused { detail });
                self.health.note_work_refused();
                return None;
            }
        };

        Some(WindowEntry {
            rows,
            batch_id,
            body_hash,
            memberships,
            edges,
            waiters: vec![reply],
        })
    }

    /// Close a commit window: one signature-sorted allocation run, one WAL record per entry, one
    /// fsync, one generation swap, then every waiter is acked. Allocation is unchanged from a
    /// single batch's, except that the sort scope is the window.
    ///
    /// A failed window burns entity ids, exactly as a failed batch did: assignment precedes the
    /// append, so ids given to a window whose append then fails are never issued again.
    ///
    /// An append or fsync failure applies nothing, in deliberate contrast to the deny path: the
    /// apply-anyway rule is written for `Delete`/`Suppress` only. A restart does not undo the
    /// refusal: replay discards every record past the last fsync.
    pub(super) fn close_window(&mut self, window: CommitWindow<Reply<Ingested>>) {
        let entries = window.len() as u64;
        let started = window.opened_at();
        let mut mark = StageMark::now();

        let closed = match self.live.with_allocator(|a| window.allocate(a)) {
            Ok((closed, tally)) => {
                // Recorded at the allocation rather than after the append: a window that allocates
                // and then fails its append has still fragmented the entity axis this much.
                self.health.record_fragmentation(tally);
                closed
            }
            Err((e, waiters)) => {
                self.fail_window_alloc(e, waiters, entries, started);
                return;
            }
        };

        mark = self.health.lap(WriteStage::Allocate, mark);

        // Mint every novel discovered-vocabulary key this window's rows carry, in place, before
        // anything is appended, against a mutable copy of the published bindings that becomes the
        // next generation's if the window survives. One copy for the whole window, not one per
        // row: a second row naming an already-minted-this-window key sees the first row's binding.
        let generation = self.generation.load_full();
        let mut vocabularies: Vocabularies = (*generation.vocabularies).clone();
        let declared_scalars = generation.bundle.manifest.declared_scalars.clone();
        // A row admitted under an earlier generation is padded here: a column declared since
        // admission appended at the tail of `declared_scalars`. Padded before the mint pass below
        // indexes by declared position, and before the append.
        let mut closed = closed;
        for entry in closed.iter_mut() {
            for row in entry.rows_mut() {
                crate::attributes::pad_to_schema(&mut row.scalars, &declared_scalars);
            }
        }
        // The group-scoped families, by the view a row names, derived once for the window.
        let scoped_by_view: FxHashMap<String, Vec<tessera_store::manifest::ScopedScalar>> =
            scoped_families_by_view(&generation.bundle.manifest);
        let mut fresh_bindings: Vec<(String, String, u32)> = Vec::new();
        let mut mint_failed: Option<MintError> = None;
        'minting: for entry in closed.iter_mut() {
            for row in entry.rows_mut() {
                for (index, declared) in declared_scalars.iter().enumerate() {
                    let Some(vocabulary) = declared.vocabulary.as_deref() else {
                        continue;
                    };
                    let WalScalar::Utf8(key) = &row.scalars[index] else {
                        continue;
                    };
                    let key = key.clone();
                    let minter = vocabularies.get_mut(vocabulary).unwrap_or_else(|| {
                        panic!(
                            "column '{}' names vocabulary '{vocabulary}', which the live bindings \
                             do not carry",
                            declared.name
                        )
                    });
                    match minter.mint(&key) {
                        Ok(Minted::Fresh(code)) => {
                            fresh_bindings.push((vocabulary.to_string(), key, code));
                            row.scalars[index] = code_at_declared_width(declared.arrow_type, code);
                        }
                        Ok(Minted::Existing(code)) => {
                            row.scalars[index] = code_at_declared_width(declared.arrow_type, code);
                        }
                        Err(e) => {
                            mint_failed = Some(e);
                            break 'minting;
                        }
                    }
                }
                // The same mint, over the row's scoped tail.
                let Some(families) = scoped_by_view.get(row.view.as_str()) else {
                    continue;
                };
                for (index, family) in families.iter().enumerate() {
                    let Some(vocabulary) = family.vocabulary.as_deref() else {
                        continue;
                    };
                    let Some(WalScalar::Utf8(key)) = row.scoped.get(index) else {
                        continue;
                    };
                    let key = key.clone();
                    let minter = vocabularies.get_mut(vocabulary).unwrap_or_else(|| {
                        panic!(
                            "scoped column family '{}' names vocabulary '{vocabulary}', which \\
                             the live bindings do not carry",
                            family.name
                        )
                    });
                    match minter.mint(&key) {
                        Ok(Minted::Fresh(code)) => {
                            fresh_bindings.push((vocabulary.to_string(), key, code));
                            row.scoped[index] = code_at_declared_width(family.arrow_type, code);
                        }
                        Ok(Minted::Existing(code)) => {
                            row.scoped[index] = code_at_declared_width(family.arrow_type, code);
                        }
                        Err(e) => {
                            mint_failed = Some(e);
                            break 'minting;
                        }
                    }
                }
            }
        }
        if let Some(e) = mint_failed {
            // Nothing has been appended yet, so the window has no effect.
            let detail = e.to_string();
            self.fail_window(
                closed,
                || ExecError::VocabularyRefused {
                    detail: detail.clone(),
                },
                entries,
                started,
            );
            return;
        }

        // Prepared before anything is appended, so a refusal spends nothing.
        let (mut mint_records, minted_per_entry) = match self.mint_records(&mut closed) {
            Ok(minted) => minted,
            Err(detail) => {
                self.fail_window(
                    closed,
                    || ExecError::LayerRefused {
                        detail: detail.clone(),
                    },
                    entries,
                    started,
                );
                return;
            }
        };
        // Runs after the vocabulary mint above, since a novel category key is a code only once
        // that pass has drawn it, and the key an artifact is named by is the value's key.
        match self.derive_records(&closed, &vocabularies) {
            Ok(records) => mint_records.extend(records),
            Err(detail) => {
                self.fail_window(
                    closed,
                    || ExecError::LayerRefused {
                        detail: detail.clone(),
                    },
                    entries,
                    started,
                );
                return;
            }
        }

        // One record per entry, appended in entries order, which is also apply order.
        let mut failed_at: Option<(usize, WalError)> = None;

        // Mint records land first, ahead of every batch record, inside the one fsync below, so a
        // mint is durable in the same commit as the rows it colours.
        for (vocabulary, key, code) in &fresh_bindings {
            if let Err(e) = self.wal.append(&WalRecord::VocabularyMint {
                vocabulary: vocabulary.clone(),
                key: key.clone(),
                code: *code,
            }) {
                failed_at = Some((0, e));
                break;
            }
        }

        // The position before each append is the only moment it can be read: afterwards the log
        // has moved on. A rotation reclaims by it below, so a failed append contributes none.
        let mut positions: Vec<u64> = Vec::with_capacity(closed.len());
        if failed_at.is_none() {
            for (i, entry) in closed.iter().enumerate() {
                let at = self.wal.position();
                if let Err(e) = self.wal.append(&entry.record) {
                    failed_at = Some((i, e));
                    break;
                }
                positions.push(at);
            }
        }

        // The joins this window's rows declared, appended behind the batch records. The
        // publications that minted come first, since an artifact must exist before anything
        // addresses it.
        let mut minted: Vec<(WalRecord, u64)> = Vec::new();
        if failed_at.is_none() {
            for record in mint_records {
                let at = self.wal.position();
                if let Err(e) = self.wal.append(&record) {
                    failed_at = Some((0, e));
                    break;
                }
                minted.push((record, at));
            }
        }
        let mut growth: Vec<(WalRecord, u64)> = Vec::new();
        if failed_at.is_none() {
            for (record, i) in growth_records(&closed) {
                let at = self.wal.position();
                if let Err(e) = self.wal.append(&record) {
                    failed_at = Some((i, e));
                    break;
                }
                growth.push((record, at));
            }
        }
        mark = self.health.lap(WriteStage::WalAppend, mark);
        // One fsync for the whole window: the amortisation half of group commit.
        if failed_at.is_none() {
            if let Err(e) = self.wal.fsync() {
                // The first waiter gets the real error arbitrarily and the rest `Poisoned`.
                failed_at = Some((0, e));
            }
        }
        self.health.lap(WriteStage::WalFsync, mark);
        self.observe_wal();
        if let Some((index, error)) = failed_at {
            self.fail_window_wal(closed, index, error, entries, started);
            return;
        }

        // Durable, not yet in force. See `pause_point`.
        self.pause_point(PauseSiteArg::AfterFsync);
        self.apply_window(&mut closed, &positions, vocabularies, &fresh_bindings);

        // After the rows are in force, never before: a membership is projected through rows.
        let (artifact_records, artifact_positions): (Vec<&WalRecord>, Vec<u64>) = minted
            .iter()
            .chain(growth.iter())
            .map(|(record, position)| (record, *position))
            .unzip();
        self.apply_artifact_records(&artifact_records, &artifact_positions);

        // Recorded after the swap, so a replay can never see it swapped but not yet indexed.
        let m = StageMark::now();
        for (entry, wal_pos) in closed.iter().zip(&positions) {
            let (batch_id, body_hash) = entry.batch_key();
            debug_assert_eq!(
                tessera_lifecycle::batch_identity(&entry.record),
                Some(tessera_lifecycle::BatchIdentity {
                    batch_id,
                    body_hash,
                    allocation: entry.entity_ids.clone(),
                }),
                "the accepted-batch index disagrees with the record it caches"
            );
            self.live.record_accepted_batch(
                batch_id.to_string(),
                body_hash,
                entry.entity_ids.clone(),
                *wal_pos,
            );
        }
        self.health.lap(WriteStage::RecordBatch, m);
        self.observe_wal();

        // Under `value_set = "open"` a typo creates a permanent object rather than being refused,
        // so the caller is told the count in its own 200 and the operator gets this line.
        let created: u64 = minted_per_entry.iter().sum();
        if created > 0 {
            tracing::info!(
                minted = created,
                artifacts = ?minted
                    .iter()
                    .flat_map(|(record, _)| match record {
                        WalRecord::ArtifactPublish { layer, level, artifacts, .. } => artifacts
                            .iter()
                            .filter_map(|a| a.key.as_ref())
                            .map(|key| format!("{key} in level {level} of {layer}"))
                            .take(8)
                            .collect::<Vec<_>>(),
                        _ => Vec::new(),
                    })
                    .collect::<Vec<_>>(),
                "an ingest batch named keys no artifact held, and this layer's value set is open, \
                 so they were created carrying nothing but their names"
            );
        }

        // A death partway through this loop leaves some waiters unacked; each gets
        // `SubmitError::ReceiptLost` → 500, never `ExecutorDead` → 503, since its ingest is
        // durably in force.
        for (entry, minted) in closed.into_iter().zip(minted_per_entry) {
            let ClosedEntry {
                entity_ids,
                mut waiters,
                ..
            } = entry;
            let last = waiters
                .pop()
                .expect("an entry always has at least one waiter");
            for waiter in waiters {
                let entity_ids = entity_ids.clone();
                waiter.ack(Ingested { entity_ids, minted });
            }
            last.ack(Ingested { entity_ids, minted });
        }

        self.health
            .record_window_service(entries, started.elapsed().as_nanos() as u64);
    }

    /// The window could not be allocated: nothing was appended, nothing applied, and the high-water
    /// mark did not move. `AllocError` is `Copy`, so every waiter gets the real one.
    pub(super) fn fail_window_alloc(
        &self,
        error: AllocError,
        waiters: Vec<Vec<Reply<Ingested>>>,
        entries: u64,
        started: std::time::Instant,
    ) {
        for entry in waiters {
            for waiter in entry {
                waiter.fail(ExecError::Alloc(error));
            }
        }
        self.health
            .record_window_service(entries, started.elapsed().as_nanos() as u64);
    }

    /// The window's append or fsync failed: apply nothing, and answer every waiter.
    pub(super) fn fail_window_wal(
        &self,
        closed: Vec<ClosedEntry<Reply<Ingested>>>,
        index: usize,
        error: WalError,
        entries: u64,
        started: std::time::Instant,
    ) {
        let mut real = Some(error);
        for (i, entry) in closed.into_iter().enumerate() {
            for (k, waiter) in entry.waiters.into_iter().enumerate() {
                // The real error goes to the entry the failure belongs to; every other waiter gets
                // `Poisoned`, which is precisely what its own append would have returned had it
                // been attempted after the failure, and what the WAL will in fact return for
                // every subsequent call.
                let e = if i == index && k == 0 {
                    real.take().unwrap_or(WalError::Poisoned)
                } else {
                    WalError::Poisoned
                };
                waiter.fail(ExecError::Wal(e));
            }
        }
        self.health
            .record_window_service(entries, started.elapsed().as_nanos() as u64);
    }

    /// Answers every waiter of a window that was refused after allocation with the same error.
    pub(super) fn fail_window(
        &self,
        closed: Vec<ClosedEntry<Reply<Ingested>>>,
        error: impl Fn() -> ExecError,
        entries: u64,
        started: std::time::Instant,
    ) {
        for entry in closed {
            for waiter in entry.waiters {
                waiter.fail(error());
            }
        }
        self.health
            .record_window_service(entries, started.elapsed().as_nanos() as u64);
    }

    /// Clone the buffer once, insert every entry in the window, publish once. The clone is
    /// O(total buffered items), so a window of k entries pays it once instead of k times.
    ///
    /// `terms` is taken out of each entry rather than borrowed, since the ack needs `entity_ids`,
    /// not terms. `vocabularies` is `close_window`'s locally mutated copy, published verbatim
    /// rather than `Arc::clone(&generation.vocabularies)`: cloning the old `Arc` here would
    /// silently discard every code this window just drew.
    pub(super) fn apply_window(
        &self,
        closed: &mut [ClosedEntry<Reply<Ingested>>],
        positions: &[u64],
        vocabularies: Vocabularies,
        mints: &[(String, String, u32)],
    ) {
        let started = std::time::Instant::now();
        let mut mark = StageMark::now();
        let generation = self.generation.load_full();
        // The suggestion index's side map, grown by exactly the keys this window minted. Nothing
        // is rebuilt: every other vocabulary's index is carried behind its `Arc`.
        let suggest = generation.suggest.with_mints(
            &tessera_analyse::SuggestionFold::new(),
            &vocabularies,
            mints,
        );
        let mut buffer = (*generation.buffer).clone();
        mark = self.health.lap(WriteStage::ApplyBufferClone, mark);

        let mut established = lock_recover(&self.live.established);
        // Updated together in one critical section, so a `/control/changes` lookup and a
        // `/v1/items` drill-down can never disagree about the same item.
        let mut established_inverse = lock_recover(&self.live.established_inverse);
        for (entry, wal_pos) in closed.iter_mut().zip(positions) {
            let terms = std::mem::take(&mut entry.terms);
            for (row, row_terms) in entry.rows().iter().zip(terms) {
                // No external id means nothing to establish.
                let mut m = StageMark::now();
                if let Some(external_id) = &row.external_id {
                    established.insert(external_id.clone(), row.entity_id);
                    m = self.health.lap(WriteStage::RowEstablished, m);
                    established_inverse.insert(row.entity_id, external_id.clone());
                    m = self.health.lap(WriteStage::RowEstablishedInv, m);
                }
                buffer.insert_row_with_terms(row, row_terms);
                let m = self.health.lap(WriteStage::RowBufferInsert, m);
                buffer.set_wal_pos(row.entity_id, &row.view, *wal_pos);
                self.health.lap(WriteStage::RowWalPos, m);
            }
        }
        drop(established);
        drop(established_inverse);
        mark = self.health.lap(WriteStage::ApplyRows, mark);

        // Published here, and at every other place buffer occupancy changes, so
        // `/control/ingest`'s occupancy bound reads a figure the executor maintains.
        self.health
            .buffered_items
            .store(buffer.len(), Ordering::SeqCst);

        let next = generation.with(|g| {
            g.overlay_version = generation.overlay_version + 1;
            g.buffer = Arc::new(buffer);
            g.vocabularies = Arc::new(vocabularies);
            // The one publication that changes the suggestion index, by the same mints that
            // changed the bindings above. Every other publication carries the index forward.
            g.suggest = suggest;
        });
        self.publish(next, started);
        self.health.lap(WriteStage::ApplySwap, mark);
    }
}
