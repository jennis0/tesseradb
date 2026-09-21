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
    /// Held in the open commit window, not yet acknowledged; a byte-identical retry joins the
    /// queue of waiters the entry will ack. There is exactly one open window.
    Held {
        window_seq: u64,
        body_hash: [u8; 32],
    },
    /// Never seen, or what an accepted batch regresses to once WAL rotation reclaims its member: a
    /// retry with an `external_id` then 409s, but one with none is re-ingested as new.
    Unknown,
}

impl BatchState {
    /// Look `batch_id` up: the durable index, then the open window, then unknown. The two sets are
    /// disjoint: a batch id enters `accepted_batches` only at `close_window`.
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

/// A closed commit window and every waiter it owes: [`ClosingWindow::fail_all`],
/// [`ClosingWindow::fail_wal`] and [`ClosingWindow::ack`] are its three exits.
pub(super) struct ClosingWindow {
    closed: Vec<ClosedEntry<Reply<Ingested>>>,
    /// Entries, not rows: what the service estimate is per.
    entries: u64,
    /// When the window opened, so the service covers the wait as well as the close.
    started: std::time::Instant,
}

impl ClosingWindow {
    pub(super) fn new(
        closed: Vec<ClosedEntry<Reply<Ingested>>>,
        entries: u64,
        started: std::time::Instant,
    ) -> Self {
        ClosingWindow {
            closed,
            entries,
            started,
        }
    }

    pub(super) fn entries(&self) -> &[ClosedEntry<Reply<Ingested>>] {
        &self.closed
    }

    pub(super) fn entries_mut(&mut self) -> &mut [ClosedEntry<Reply<Ingested>>] {
        &mut self.closed
    }

    /// Before the append: nothing was appended or applied, and every waiter gets the same reason.
    pub(super) fn fail_all(self, health: &ExecutorHealth, error: impl Fn() -> ExecError) {
        let waiters = self.closed.into_iter().map(|e| e.waiters).collect();
        refuse_waiters(health, waiters, self.entries, self.started, error);
    }

    /// The append or fsync failed: apply nothing, and blame [`Blame`]'s rule for every waiter but
    /// the one carrying the real error.
    pub(super) fn fail_wal(self, health: &ExecutorHealth, blamed: usize, error: WalError) {
        let mut blame = Blame::new(blamed, error);
        for (i, entry) in self.closed.into_iter().enumerate() {
            for (k, waiter) in entry.waiters.into_iter().enumerate() {
                // A joined retry was never appended separately, so it has nothing else to be told.
                let e = if k == 0 { blame.at(i) } else { WalError::Poisoned };
                waiter.fail(ExecError::Wal(e));
            }
        }
        health.record_window_service(self.entries, self.started.elapsed().as_nanos() as u64);
    }

    /// Committed: every waiter of an entry gets that entry's ids and the artifacts it created. A
    /// death partway leaves some waiters unacked; each gets a 500, not a 503, since durable.
    pub(super) fn ack(self, health: &ExecutorHealth, minted_per_entry: Vec<u64>) {
        for (entry, minted) in self.closed.into_iter().zip(minted_per_entry) {
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
        health.record_window_service(self.entries, self.started.elapsed().as_nanos() as u64);
    }
}

/// What a window's vocabulary pass drew.
pub(super) struct MintedCodes {
    /// The bindings the window publishes if it survives, the live ones plus whatever it drew.
    pub(super) vocabularies: Vocabularies,
    /// `(vocabulary, key, code)` per key bound, in the order the durable records are appended.
    pub(super) fresh: Vec<(String, String, u32)>,
}

/// Under `value_set = "open"` a typo creates a permanent object rather than a refusal, so the
/// operator gets this line.
fn log_minted_artifacts(minted_per_entry: &[u64], mint_records: &[WalRecord]) {
    let created: u64 = minted_per_entry.iter().sum();
    if created == 0 {
        return;
    }
    tracing::info!(
        minted = created,
        artifacts = ?mint_records
            .iter()
            .flat_map(|record| match record {
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

/// Draw a code for every novel vocabulary key one positional cell list carries, and rewrite each
/// cell to its code at the column's declared width.
fn mint_cells<'a>(
    cells: &mut [WalScalar],
    columns: impl Iterator<Item = (&'a str, Option<&'a str>, ScalarType)>,
    vocabularies: &mut Vocabularies,
    fresh: &mut Vec<(String, String, u32)>,
) -> std::result::Result<(), MintError> {
    for (index, (name, vocabulary, arrow_type)) in columns.enumerate() {
        let Some(vocabulary) = vocabulary else {
            continue;
        };
        let code = {
            let Some(WalScalar::Utf8(key)) = cells.get(index) else {
                continue;
            };
            let held = vocabularies.get(vocabulary).unwrap_or_else(|| {
                panic!(
                    "column '{name}' names vocabulary '{vocabulary}', which the live bindings do \
                     not carry"
                )
            });
            // Asked of the shared minter, so a window binding no novel key copies none of them.
            match held.code_of(key) {
                Some(code) => code,
                None => {
                    let key = key.clone();
                    let minter = vocabularies
                        .get_mut(vocabulary)
                        .expect("the minter this call just read");
                    match minter.mint(&key)? {
                        Minted::Fresh(code) => {
                            fresh.push((vocabulary.to_string(), key, code));
                            code
                        }
                        Minted::Existing(code) => code,
                    }
                }
            }
        };
        cells[index] = code_at_declared_width(arrow_type, code);
    }
    Ok(())
}

/// The window could not be allocated: nothing was appended, nothing applied, and the high-water
/// mark did not move. The one exit outside [`ClosingWindow`], since allocation is what failed.
fn fail_allocation(
    health: &ExecutorHealth,
    error: AllocError,
    waiters: Vec<Vec<Reply<Ingested>>>,
    entries: u64,
    started: std::time::Instant,
) {
    refuse_waiters(health, waiters, entries, started, || ExecError::Alloc(error));
}

/// Answer every waiter of a window that will not commit, and time the window.
fn refuse_waiters(
    health: &ExecutorHealth,
    waiters: Vec<Vec<Reply<Ingested>>>,
    entries: u64,
    started: std::time::Instant,
    error: impl Fn() -> ExecError,
) {
    for entry in waiters {
        for waiter in entry {
            waiter.fail(error());
        }
    }
    health.record_window_service(entries, started.elapsed().as_nanos() as u64);
}

/// What [`Executor::admit_ingest`] did with a submission, as far as the drain loop needs to know.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum Admission {
    /// It is in the window (or its waiters).
    Admitted,
    /// It was answered outright (a replay, a join or a 409) and nothing was added to the window.
    Answered,
    /// A conflicting external id forced the open window to close; the pass must yield to a deny.
    YieldedAfterClose,
}

impl Executor {
    /// Drains the work queue into one commit window and closes it, returning whether anything was
    /// done. Closes at `commit_window_max_rows`, an empty queue, or a conflicting external id.
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
            // Anything but a lifecycle command reads or replaces state the window is holding back.
            if !matches!(work, ExecutorWork::Lifecycle(_)) && !window.is_empty() {
                window = self.close_and_reopen(window);
            }
            let command = match work {
                ExecutorWork::Lifecycle(command) => command,
                ExecutorWork::PublishGeometry {
                    publication,
                    respond,
                } => {
                    let _ = respond.send(self.publish_geometry(publication));
                    self.health.note_work_finished();
                    did_work = true;
                    continue;
                }
                #[cfg(feature = "fault-injection")]
                ExecutorWork::ForgetSuggestionIndex {
                    vocabulary,
                    respond,
                } => {
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
                // Tolerable while a window is open: none of these touch the buffer or the swap.
                self.execute(command);
                self.health.note_work_finished();
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

    /// Close `window` and return its replacement, stamped after the close: stamped first, the
    /// replacement would charge its predecessor's whole service to itself.
    pub(super) fn close_and_reopen(&mut self, window: CommitWindow<Reply<Ingested>>) -> CommitWindow<Reply<Ingested>> {
        self.close_window(window);
        CommitWindow::new(self.next_window_seq())
    }

    pub(super) fn next_window_seq(&mut self) -> u64 {
        self.window_seq += 1;
        self.window_seq
    }

    /// The batch-id state machine, evaluated on the executor rather than the handler: between a
    /// handler check and the enqueue the window can close, so a retry that saw unknown could
    /// double-allocate into a fresh window.
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
                self.health.note_work_finished();
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
                self.health.note_work_finished();
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

    /// The external-id admission check. `None` means the caller has already been answered. This
    /// reads state written at apply, which is why an entry naming an id the open window holds must
    /// close it first.
    pub(super) fn admit(
        &mut self,
        rows: Vec<UnallocatedRow>,
        batch_id: String,
        body_hash: [u8; 32],
        artifacts: tessera_lifecycle::BatchArtifacts,
        reply: Reply<Ingested>,
    ) -> Option<WindowEntry<Reply<Ingested>>> {
        // From the same generation the apply below clones, so the exemption cannot race a delete.
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
                self.health.note_work_finished();
                return None;
            }
        }
        drop(generation);
        if collisions > 0 {
            reply.fail(ExecError::DuplicateExternalId { count: collisions },
            );
            self.health.note_work_finished();
            return None;
        }

        let (memberships, edges) = match self.resolve_memberships(&artifacts) {
            Ok(resolved) => resolved,
            Err(detail) => {
                reply.fail(ExecError::LayerRefused { detail });
                self.health.note_work_finished();
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

    /// Pad every row to schema and draw a code for every discovered-vocabulary key, into one
    /// mutable copy of the bindings shared by the whole window.
    pub(super) fn mint_window_codes(
        &self,
        closing: &mut ClosingWindow,
    ) -> std::result::Result<MintedCodes, MintError> {
        let generation = self.generation.load_full();
        let mut vocabularies: Vocabularies = (*generation.vocabularies).clone();
        let declared_scalars = &generation.bundle.manifest.declared_scalars;
        // The group-scoped families, by the view a row names, derived once for the window.
        let scoped_by_view: FxHashMap<String, Vec<tessera_store::manifest::ScopedScalar>> =
            scoped_families_by_view(&generation.bundle.manifest);
        let mut fresh: Vec<(String, String, u32)> = Vec::new();
        for entry in closing.entries_mut() {
            for row in entry.rows_mut() {
                // A column declared since admission is appended at the tail of `declared_scalars`.
                crate::attributes::pad_to_schema(&mut row.scalars, declared_scalars);
                mint_cells(
                    &mut row.scalars,
                    declared_scalars
                        .iter()
                        .map(|d| (d.name.as_str(), d.vocabulary.as_deref(), d.arrow_type)),
                    &mut vocabularies,
                    &mut fresh,
                )?;
                let Some(families) = scoped_by_view.get(row.view.as_str()) else {
                    continue;
                };
                mint_cells(
                    &mut row.scoped,
                    families
                        .iter()
                        .map(|f| (f.name.as_str(), f.vocabulary.as_deref(), f.arrow_type)),
                    &mut vocabularies,
                    &mut fresh,
                )?;
            }
        }
        Ok(MintedCodes {
            vocabularies,
            fresh,
        })
    }

    /// Close a commit window: allocate, then one fsync over the vocabulary mints, the entries, the
    /// artifact publications and the growths against them, in that order, since a mint must be
    /// durable before the rows it colours and a publication must exist before anything addresses
    /// it. A failure applies nothing, so ids given to a failed window are never reissued.
    pub(super) fn close_window(&mut self, window: CommitWindow<Reply<Ingested>>) {
        let entries = window.len() as u64;
        let started = window.opened_at();
        let mut mark = StageMark::now();

        let closed = match self.live.with_allocator(|a| window.allocate(a)) {
            Ok((closed, tally)) => {
                // Recorded at allocation: a failed append still fragmented the entity axis.
                self.health.record_fragmentation(tally);
                closed
            }
            Err((error, waiters)) => {
                fail_allocation(&self.health, error, waiters, entries, started);
                return;
            }
        };
        let mut closing = ClosingWindow::new(closed, entries, started);

        mark = self.health.lap(WriteStage::Allocate, mark);

        let MintedCodes {
            vocabularies,
            fresh: fresh_bindings,
        } = match self.mint_window_codes(&mut closing) {
            Ok(minted) => minted,
            Err(e) => {
                let detail = e.to_string();
                closing.fail_all(&self.health, || ExecError::VocabularyRefused {
                    detail: detail.clone(),
                });
                return;
            }
        };

        let (mut mint_records, minted_per_entry) = match self.mint_records(closing.entries_mut()) {
            Ok(minted) => minted,
            Err(detail) => {
                closing.fail_all(&self.health, || ExecError::LayerRefused {
                    detail: detail.clone(),
                });
                return;
            }
        };
        // After the vocabulary mint above, since an artifact's key is a code only once drawn.
        match self.derive_records(closing.entries(), &vocabularies) {
            Ok(records) => mint_records.extend(records),
            Err(detail) => {
                closing.fail_all(&self.health, || ExecError::LayerRefused {
                    detail: detail.clone(),
                });
                return;
            }
        }

        // A window carrying no join row reads no artifact.
        let restating = joining_entities(closing.entries());
        let prepared = if restating.is_empty() {
            growth_records(closing.entries(), None)
        } else {
            self.live.with_artifacts(|store| {
                growth_records(
                    closing.entries(),
                    Some(HeldMembers {
                        store,
                        restating: Some(&restating),
                    }),
                )
            })
        };
        let growth = match prepared {
            Ok(growth) => growth,
            Err(detail) => {
                closing.fail_all(&self.health, || ExecError::LayerRefused {
                    detail: detail.clone(),
                });
                return;
            }
        };

        let vocabulary_records: Vec<WalRecord> = fresh_bindings
            .iter()
            .map(|(vocabulary, key, code)| WalRecord::VocabularyMint {
                vocabulary: vocabulary.clone(),
                key: key.clone(),
                code: *code,
            })
            .collect();
        let entries_at = vocabulary_records.len();
        let artifacts_at = entries_at + closing.entries().len();
        let growth_at = artifacts_at + mint_records.len();
        let durable: Vec<&WalRecord> = vocabulary_records
            .iter()
            .chain(closing.entries().iter().map(|entry| &entry.record))
            .chain(mint_records.iter())
            .chain(growth.iter().map(|(record, _)| record))
            .collect();

        let positions = match self.append_and_sync(&durable, Some(mark)) {
            Ok(positions) => positions,
            Err(failure) => {
                // A mint, a publication, or a sync all belong to the window, not to one entry.
                let blamed = match failure {
                    Undurable::Append { at, .. } if (entries_at..artifacts_at).contains(&at) => {
                        at - entries_at
                    }
                    Undurable::Append { at, .. } if at >= growth_at => growth[at - growth_at].1,
                    _ => 0,
                };
                drop(durable);
                closing.fail_wal(&self.health, blamed, failure.into_error());
                return;
            }
        };
        drop(durable);

        // Durable, not yet in force. See `pause_point`.
        self.pause_point(PauseSiteArg::AfterFsync);
        self.apply_window(
            closing.entries_mut(),
            &positions[entries_at..artifacts_at],
            vocabularies,
            &fresh_bindings,
        );

        // After the rows are in force, never before: a membership is projected through rows.
        let artifact_records: Vec<&WalRecord> = mint_records
            .iter()
            .chain(growth.iter().map(|(record, _)| record))
            .collect();
        self.apply_artifact_records(&artifact_records, &positions[artifacts_at..], Publish::AtTick);

        self.record_accepted_batches(closing.entries(), &positions[entries_at..artifacts_at]);
        log_minted_artifacts(&minted_per_entry, &mint_records);
        closing.ack(&self.health, minted_per_entry);
    }

    /// Index every entry of a committed window by its batch id, after the swap.
    pub(super) fn record_accepted_batches(
        &mut self,
        closed: &[ClosedEntry<Reply<Ingested>>],
        positions: &[u64],
    ) {
        let m = StageMark::now();
        for (entry, wal_pos) in closed.iter().zip(positions) {
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
    }

    /// Clone the buffer once, insert every entry in the window, publish once. The clone is
    /// O(total buffered items), so a window of k entries pays it once. `vocabularies` is
    /// `close_window`'s own mutated copy, published verbatim, not cloned from the published `Arc`.
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
        // Grown by exactly the keys this window minted; every other index carries forward.
        let suggest = generation.suggest.with_mints(
            &tessera_analyse::SuggestionFold::new(),
            &vocabularies,
            mints,
        );
        let mut buffer = (*generation.buffer).clone();
        mark = self.health.lap(WriteStage::ApplyBufferClone, mark);

        // What the buffered-row lists grow by: every entity this window buffered a row for.
        let mut inserted: Vec<EntityId> = Vec::new();
        let mut established = lock_recover(&self.live.established);
        // Updated together in one critical section, so the two can never disagree about an item.
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
                inserted.push(row.entity_id);
            }
        }
        drop(established);
        drop(established_inverse);
        mark = self.health.lap(WriteStage::ApplyRows, mark);

        // Published here, and everywhere else buffer occupancy changes.
        self.health
            .buffered_items
            .store(buffer.len(), Ordering::SeqCst);

        let next = generation.with_buffer(Arc::new(buffer), &inserted, |g| {
            g.overlay_version = generation.overlay_version + 1;
            g.vocabularies = Arc::new(vocabularies);
            g.suggest = suggest;
        });
        self.publish(next, started);
        self.health.lap(WriteStage::ApplySwap, mark);
    }
}
