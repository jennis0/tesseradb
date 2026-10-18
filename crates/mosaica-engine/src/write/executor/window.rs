use super::*;

use mosaica_types::ItemAt;

/// What `/control/ingest`'s batch id means to this executor: durably accepted, held in the open
/// commit window, or never seen. Computed by [`BatchState::of`] on the executor thread only.
pub(super) enum BatchState {
    /// Durably accepted: the WAL record is fsynced, the rows are applied and the receipt is
    /// recorded. Same bytes replays the receipt; different bytes is a `409`.
    Accepted {
        body_hash: [u8; 32],
        receipt: Vec<mosaica_lifecycle::RowReceipt>,
    },
    /// Held in the open commit window, not yet acknowledged; a byte-identical retry joins the
    /// queue of waiters the entry will ack. There is exactly one open window.
    Held {
        window_seq: u64,
        body_hash: [u8; 32],
    },
    /// Never seen, or what an accepted batch regresses to once WAL rotation reclaims its member:
    /// a retry is then resolved again as a new batch.
    Unknown,
}

impl BatchState {
    /// Look `batch_id` up: the durable index, then the open window, then unknown. The two sets are
    /// disjoint: a batch id enters `accepted_batches` only at `close_window`.
    pub(super) fn of<W>(live: &LiveState, window: &CommitWindow<W>, batch_id: &str) -> BatchState {
        if let Some((body_hash, receipt)) = live.accepted_batch(batch_id) {
            return BatchState::Accepted { body_hash, receipt };
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
    /// Dropped by a failing exit before it answers a waiter, so a retry finds the ids given back.
    placing: Placing,
}

/// What a window's placement did: the ids it took from each shard, and the shards it sealed and
/// opened. Unless the window commits, every shard has its ids back and its seal undone, and no
/// opened shard takes writes.
pub(super) struct Placing {
    /// Each shard held before the window, with what the window took from it.
    taken: Vec<(Arc<ShardWrite>, mosaica_lifecycle::alloc::Taken)>,
    placed: mosaica_lifecycle::Placed,
    committed: bool,
}

impl Placing {
    /// `shards` are those the window was allocated over, in the order it was given them.
    pub(super) fn new(
        shards: &[Arc<ShardWrite>],
        mut placed: mosaica_lifecycle::Placed,
    ) -> Placing {
        let taken = std::mem::take(&mut placed.taken);
        debug_assert!(shards
            .iter()
            .map(|shard| shard.id)
            .eq(taken.iter().map(|(id, _)| *id)));
        Placing {
            taken: shards
                .iter()
                .cloned()
                .zip(taken)
                .map(|(shard, (_, taken))| (shard, taken))
                .collect(),
            placed,
            committed: false,
        }
    }

    /// The shards the window opened, ascending.
    pub(super) fn opened(&self) -> Vec<ShardId> {
        self.placed.opened.iter().map(|(shard, _)| *shard).collect()
    }

    /// The records that open and seal them, appended ahead of the window's entries.
    pub(super) fn records(&self) -> Vec<WalRecord> {
        self.placed.records()
    }

    /// The window is durable: its seals stand and the shards it opened take writes. Answers every
    /// shard it sealed, in the order they sealed, those it opened among them.
    pub(super) fn commit(&mut self, live: &LiveState) -> Vec<ShardId> {
        self.committed = true;
        let sealed = std::mem::take(&mut self.placed.sealed);
        let opened = std::mem::take(&mut self.placed.opened);
        live.add_shards(
            opened
                .into_iter()
                .map(|(shard, alloc)| ShardWrite::new(shard, alloc))
                .collect(),
        );
        sealed
    }
}

impl Drop for Placing {
    fn drop(&mut self) {
        if !self.committed {
            for (shard, taken) in self.taken.drain(..) {
                shard.with_allocator(|alloc| alloc.give_back(taken));
            }
        }
    }
}

/// The shards a window opens, written and opened before anything names them.
pub(super) struct OpenedShards {
    /// The live bundle, serving each of them as well.
    bundle: Arc<mosaica_store::read::Bundle>,
    /// Each one's parts, as a generation first serves it.
    parts: Vec<(ShardId, crate::ShardParts)>,
    /// Each one's member packs, one beside each record pack, listing nothing.
    members: Vec<(ShardId, Vec<mosaica_store::manifest::MembershipExtent>)>,
    /// Those packs' digests.
    member_files: std::collections::BTreeMap<String, mosaica_store::manifest::FileDigest>,
}

impl ClosingWindow {
    pub(super) fn new(
        closed: Vec<ClosedEntry<Reply<Ingested>>>,
        placing: Placing,
        entries: u64,
        started: std::time::Instant,
    ) -> Self {
        ClosingWindow {
            closed,
            entries,
            started,
            placing,
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
        drop(self.placing);
        let waiters = self.closed.into_iter().map(|e| e.waiters).collect();
        refuse_waiters(health, waiters, self.entries, self.started, error);
    }

    /// The append or fsync failed: apply nothing, and blame [`Blame`]'s rule for every waiter but
    /// the one carrying the real error.
    pub(super) fn fail_wal(self, health: &ExecutorHealth, blamed: usize, error: WalError) {
        drop(self.placing);
        let mut blame = Blame::new(blamed, error);
        for (i, entry) in self.closed.into_iter().enumerate() {
            for (k, waiter) in entry.waiters.into_iter().enumerate() {
                // A joined retry was never appended separately, so it has nothing else to be told.
                let e = if k == 0 {
                    blame.at(i)
                } else {
                    WalError::Poisoned
                };
                waiter.fail(ExecError::Wal(e));
            }
        }
        health.record_window_service(self.entries, self.started.elapsed().as_nanos() as u64);
    }

    /// Committed: every waiter of an entry gets that entry's receipt and the artifacts it
    /// created. A death partway leaves some waiters unacked; each gets a 500, not a 503, since
    /// durable.
    pub(super) fn ack(
        self,
        health: &ExecutorHealth,
        minted_per_entry: Vec<u64>,
        joined_per_entry: Vec<u64>,
    ) {
        let counts = minted_per_entry.into_iter().zip(joined_per_entry);
        for (entry, (minted, joined)) in self.closed.into_iter().zip(counts) {
            let receipt = entry.receipt().to_vec();
            for waiter in entry.waiters {
                waiter.ack(Ingested {
                    receipt: receipt.clone(),
                    minted,
                    joined,
                    replayed: false,
                });
            }
        }
        health.record_window_service(self.entries, self.started.elapsed().as_nanos() as u64);
    }
}

/// What a vocabulary pass drew over a commit window's rows.
pub(super) struct MintedCodes {
    /// The bindings published if the write commits: the live ones plus whatever the pass drew.
    pub(super) vocabularies: Vocabularies,
    /// `(vocabulary, key, code)` per key bound, in the order the durable records are appended.
    pub(super) fresh: Vec<(String, String, u32)>,
}

impl MintedCodes {
    /// One durable record per key bound, to be appended before any record carrying its code.
    pub(super) fn records(&self) -> Vec<WalRecord> {
        self.fresh
            .iter()
            .map(|(vocabulary, key, code)| WalRecord::VocabularyMint {
                vocabulary: vocabulary.clone(),
                key: key.clone(),
                code: *code,
            })
            .collect()
    }
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
        cells[index] = mosaica_store::vocabulary::code_value(arrow_type, code);
    }
    Ok(())
}

/// The window could not be allocated: nothing was appended, nothing applied, and the high-water
/// mark did not move. The one exit outside [`ClosingWindow`], since allocation is what failed.
/// Admission refuses a batch whose edits pass the end of a shard's ids or whose new items need a
/// shard past the last number; this is the backstop.
fn fail_allocation(
    health: &ExecutorHealth,
    error: mosaica_lifecycle::AllocError,
    waiters: Vec<Vec<Reply<Ingested>>>,
    entries: u64,
    started: std::time::Instant,
) {
    refuse_waiters(health, waiters, entries, started, || ExecError::from(error));
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
    /// A submission touching what the open window holds forced it to close; the pass must yield
    /// to a deny.
    YieldedAfterClose,
}

impl Executor {
    /// Drains the work queue into one commit window and closes it, returning whether anything was
    /// done. Closes at `commit_window_max_rows`, an empty queue, or a submission touching what the
    /// window holds.
    pub(super) fn run_work_pass(&mut self) -> bool {
        let max_rows = self.health.commit_window_max_rows();
        #[cfg(feature = "fault-injection")]
        self.deps.switches.hold_work_pass_if_paused();
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
            let Command::Ingest { submission, reply } = command else {
                // A declaration reads the unique values the window has not applied, a shard size
                // is checked against the high water the window has yet to take, and a change to
                // the view roster changes the views the window's rows are going into, so the
                // window closes first. Every other command is tolerable while a window is open: an
                // edit's close carries the memberships and generating sets it finds then.
                if matches!(
                    command,
                    Command::DeclareAttribute { .. }
                        | Command::DeclareShards { .. }
                        | Command::CreateView { .. }
                        | Command::DropView { .. }
                        | Command::CreateViewGroup { .. }
                        | Command::CreatePlainView { .. }
                ) && !window.is_empty()
                {
                    window = self.close_and_reopen(window);
                }
                self.execute(command);
                did_work = true;
                continue;
            };

            let admitted;
            let m = StageMark::now();
            (window, admitted) = self.admit_ingest(window, submission, reply);
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
    pub(super) fn close_and_reopen(
        &mut self,
        window: CommitWindow<Reply<Ingested>>,
    ) -> CommitWindow<Reply<Ingested>> {
        self.close_window(window);
        CommitWindow::new(self.next_window_seq())
    }

    pub(super) fn next_window_seq(&mut self) -> u64 {
        self.window_seq += 1;
        self.window_seq
    }

    /// The batch-id state machine, evaluated on the executor rather than the handler: between a
    /// handler check and the enqueue the window can close, so a retry that saw unknown could
    /// be admitted into a fresh window twice.
    pub(super) fn admit_ingest(
        &mut self,
        mut window: CommitWindow<Reply<Ingested>>,
        submission: IngestSubmission,
        reply: Reply<Ingested>,
    ) -> (CommitWindow<Reply<Ingested>>, Admission) {
        match BatchState::of(&self.live, &window, &submission.batch_id) {
            BatchState::Accepted {
                body_hash: prev_hash,
                receipt,
            } => {
                if prev_hash == submission.body_hash {
                    // A replay mints nothing: this batch's keys were created when first accepted.
                    reply.ack(Ingested {
                        receipt,
                        minted: 0,
                        joined: 0,
                        replayed: true,
                    });
                } else {
                    reply.fail(ExecError::BatchConflict {
                        batch_id: submission.batch_id,
                    });
                }
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
                if prev_hash == submission.body_hash {
                    let joined = window.join(&submission.batch_id, reply);
                    debug_assert!(joined, "`held` just answered for this batch id");
                } else {
                    // The 409 reaches the retry, not the held original, which is still owed its ack.
                    reply.fail(ExecError::BatchConflict {
                        batch_id: submission.batch_id,
                    });
                }
                (window, Admission::Answered)
            }
            BatchState::Unknown => {
                let mut admission = Admission::Admitted;
                // What the open window touches is written at its close, and the re-check below
                // reads what was written.
                let joined = submission.slots.iter().filter_map(|slot| match slot {
                    mosaica_lifecycle::Slot::Joined { item, .. } => Some(*item),
                    _ => None,
                });
                let edited = submission.edits.iter().map(|e| e.edit.item());
                let claims = mosaica_lifecycle::WindowClaims::of(
                    &submission.rows,
                    edited.chain(joined),
                    submission.keys.clone(),
                );
                if window.conflicts(&claims) {
                    window = self.close_and_reopen(window);
                    admission = Admission::YieldedAfterClose;
                }
                // A batch that does not fit beside the open window closes it, and is refused if it
                // does not fit alone.
                let wanted = mosaica_lifecycle::window::wanted(
                    &submission.rows,
                    submission.edits.iter().map(|e| e.edit.shard),
                );
                let live = Arc::clone(&self.live);
                let room = |window: &CommitWindow<Reply<Ingested>>| {
                    let mut total = window.wanted().clone();
                    total.add(wanted.clone());
                    live.window_room(&total.edits, total.items)
                };
                if !window.is_empty() && room(&window).is_err() {
                    window = self.close_and_reopen(window);
                    admission = Admission::YieldedAfterClose;
                }
                if let Err(refused) = room(&window) {
                    reply.fail(ExecError::from(refused));
                    return (window, admission);
                }
                if let Some(entry) = self.admit(submission, reply) {
                    if window.is_empty() {
                        // Armed at the first entry: an empty window is never closed.
                        self.health.mark_work_started(window.opened_at());
                    }
                    window.push(entry, claims);
                }
                (window, admission)
            }
        }
    }

    /// The re-check of what the handler resolved, against what has changed since, from memory,
    /// and the artifacts the batch names. `None` means the caller has already been answered.
    pub(super) fn admit(
        &mut self,
        submission: IngestSubmission,
        reply: Reply<Ingested>,
    ) -> Option<WindowEntry<Reply<Ingested>>> {
        let IngestSubmission {
            rows,
            edits,
            slots,
            keys,
            batch_id,
            body_hash,
            artifacts,
            unique_seq,
            over_bound,
        } = submission;
        let generation = self.generation.load();
        let creates = rows.iter().any(|row| row.join.is_none()) || !edits.is_empty();
        // Each item is read in its own shard; one in a shard the generation no longer holds is
        // gone.
        let deleted = |item: ItemAt| crate::unique::deleted_in(&generation, item);
        let stale = crate::unique::moved_since(&generation, &keys, unique_seq, creates)
                || edits.iter().any(|submitted| {
                    let old = submitted.edit.item();
                    deleted(old)
                        || crate::write::joined::views_holding(
                            generation.shard_of(old.shard),
                            old.entity,
                        ) != submitted.held_views
                })
                || slots.iter().any(|slot| match slot {
                    mosaica_lifecycle::Slot::Unchanged { item, .. }
                    | mosaica_lifecycle::Slot::Joined { item, .. } => deleted(*item),
                    _ => false,
                })
                || rows.iter().any(|row| match row.join {
                    Some(item) => {
                        deleted(item)
                            || crate::edited::holds_in_view(
                                generation.shard_of(item.shard),
                                item.entity,
                                &row.view,
                            )
                    }
                    None => false,
                });
        // An entity the handler resolved can since have been retired by a fold, freed, and issued
        // to another item: each must still hold the item the handler resolved it to, and its
        // number must still be at the tenancy the identifier carries.
        let stale = stale || {
            let key = &self.deps.identity_key;
            let number_of = |mosaica_id: u64| {
                crate::edited::number_named(
                    key,
                    &generation,
                    mosaica_types::MosaicaId::new(mosaica_id),
                )
                .map(|(shard, number)| (shard.id(), number))
            };
            let mut named: Vec<(ItemAt, Option<(ShardId, EntityId)>)> = Vec::new();
            for slot in &slots {
                match slot {
                    mosaica_lifecycle::Slot::Unchanged { item, mosaica_id }
                    | mosaica_lifecycle::Slot::Joined { item, mosaica_id } => {
                        named.push((*item, number_of(*mosaica_id)));
                    }
                    mosaica_lifecycle::Slot::Edited { edit, mosaica_id } => {
                        let old = edits.get(*edit as usize).map(|e| e.edit.item());
                        named.extend(old.map(|old| (old, number_of(*mosaica_id))));
                    }
                    mosaica_lifecycle::Slot::Written {
                        row,
                        mosaica_id: Some(mosaica_id),
                    } => {
                        if let Some(item) = rows.get(*row as usize).and_then(|r| r.join) {
                            named.push((item, number_of(*mosaica_id)));
                        }
                    }
                    _ => {}
                }
            }
            let by_shard = mosaica_types::by_shard(
                named
                    .iter()
                    .enumerate()
                    .map(|(at, (item, _))| (item.shard, at)),
            );
            by_shard.into_iter().any(|(id, ats)| {
                let Some(shard) = generation.shard(id) else {
                    return true;
                };
                let entities: Vec<EntityId> = ats.iter().map(|&at| named[at].0.entity).collect();
                match crate::edited::numbers_of(shard, &entities) {
                    Ok(numbers) => ats
                        .iter()
                        .zip(numbers)
                        .any(|(&at, now)| named[at].1 != Some((id, now))),
                    Err(_) => true,
                }
            })
        };
        drop(generation);
        if stale {
            reply.fail(ExecError::Stale);
            return None;
        }
        let (memberships, edges) = match self.resolve_memberships(&artifacts) {
            Ok(resolved) => resolved,
            Err(detail) => {
                reply.fail(ExecError::LayerRefused { detail });
                return None;
            }
        };

        Some(WindowEntry {
            rows,
            edits: edits.into_iter().map(|e| e.edit).collect(),
            slots,
            over_bound,
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
        let scoped_by_view: FxHashMap<String, Vec<mosaica_store::manifest::ScopedScalar>> =
            scoped_families_by_view(&generation.bundle.manifest);
        let mut fresh: Vec<(String, String, u32)> = Vec::new();
        for entry in closing.entries_mut() {
            let (rows, edits) = entry.rows_and_edits_mut();
            for row in rows
                .iter_mut()
                .chain(edits.iter_mut().flat_map(|e| e.rows.iter_mut()))
            {
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
    /// it. A failure applies nothing, and gives every id the window took back to its shard.
    pub(super) fn close_window(&mut self, window: CommitWindow<Reply<Ingested>>) {
        let entries = window.len() as u64;
        let started = window.opened_at();
        let mut mark = StageMark::now();

        // Each allocator's freed ids are at its shard's index's tenancies: a fold swaps its index
        // in before it releases the ids it freed, and both run on this thread. A shard the window
        // opens has no index, and its every number is at tenancy 0.
        let generation = self.generation.load_full();
        let identity = self.deps.identity_key;
        let mosaica_id_of = |item: ItemAt| {
            let tenancy = generation
                .shard(item.shard)
                .map_or(mosaica_types::Tenancy::ZERO, |shard| {
                    shard.tenancy.of(item.entity)
                });
            crate::edited::id_at(&identity, item.shard, tenancy, item.entity)
                .map(|id| id.raw())
                .expect("the allocator issues entities inside the identity space")
        };
        let shards = self.live.shards();
        let allocated = {
            let mut allocators = lock_allocators(&shards);
            window.allocate(
                &mut mosaica_lifecycle::Allocators {
                    shards: shards
                        .iter()
                        .zip(allocators.iter_mut())
                        .map(|(shard, alloc)| (shard.id, &mut **alloc))
                        .collect(),
                    size: self.live.shard_size(),
                    next_id: self.live.next_shard_id(),
                },
                mosaica_id_of,
            )
        };
        drop(generation);
        let (closed, tally, placing) = match allocated {
            Ok((closed, tally, placed)) => (closed, tally, Placing::new(&shards, placed)),
            Err((error, waiters)) => {
                fail_allocation(&self.health, error, waiters, entries, started);
                return;
            }
        };
        let mut closing = ClosingWindow::new(closed, placing, entries, started);
        // Written before anything names them; a failure leaves files nothing names.
        let opening = closing.placing.opened();
        let opened = match self.open_shards(&opening) {
            Ok(opened) => opened,
            Err(e) => {
                tracing::error!(
                    error = %e,
                    "ALARM: a shard this window opens could not be written; nothing of the \
                     window is in force"
                );
                let shards: Vec<String> = opening.iter().map(ToString::to_string).collect();
                let detail = format!(
                    "the files of the shards this window opens ({}) could not be written ({e})",
                    shards.join(", ")
                );
                closing.fail_all(&self.health, || ExecError::Store {
                    detail: detail.clone(),
                });
                return;
            }
        };

        mark = self.health.lap(WriteStage::Allocate, mark);

        let minted = match self.mint_window_codes(&mut closing) {
            Ok(minted) => minted,
            Err(e) => {
                let detail = e.to_string();
                closing.fail_all(&self.health, || ExecError::VocabularyRefused {
                    detail: detail.clone(),
                });
                return;
            }
        };
        let vocabulary_records = minted.records();
        let MintedCodes {
            vocabularies,
            fresh: fresh_bindings,
        } = minted;

        mark = self.health.lap(WriteStage::VocabularyMint, mark);

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
        let rows = closing.entries().iter().flat_map(|entry| {
            entry
                .rows()
                .iter()
                .chain(entry.edits().iter().filter_map(|edit| edit.rows.first()))
                .filter(|row| !row.join)
                .map(|row| row.scalars.as_slice())
        });
        match self.derive_records(rows, &vocabularies) {
            Ok(records) => mint_records.extend(records),
            Err(detail) => {
                closing.fail_all(&self.health, || ExecError::LayerRefused {
                    detail: detail.clone(),
                });
                return;
            }
        }

        // A window carrying no join row reads no artifact.
        let restating = joining_items(closing.entries());
        let (prepared, joined_per_entry) = if restating.is_empty() {
            (
                growth_records(closing.entries(), None),
                joined_per_entry(closing.entries(), None),
            )
        } else {
            self.live.with_artifacts(|store| {
                (
                    growth_records(
                        closing.entries(),
                        Some(HeldMembers {
                            store,
                            restating: Some(&restating),
                        }),
                    ),
                    joined_per_entry(closing.entries(), Some(store)),
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

        // An edited item keeps its suppression and every membership and generating set it is in,
        // read here, on the thread that changes them, so nothing can move between the read and
        // the append.
        let generation = self.generation.load_full();
        let mut pairs: Vec<(ShardId, (EntityId, EntityId))> = Vec::new();
        let mut first_edit: Option<usize> = None;
        for (index, entry) in closing.entries_mut().iter_mut().enumerate() {
            let (_, edits) = entry.rows_and_edits_mut();
            for edit in edits.iter_mut() {
                edit.suppressed = generation
                    .shard_of(edit.shard)
                    .overlay
                    .is_suppressed(edit.old);
                pairs.push((edit.shard, (edit.old, edit.rows[0].entity_id)));
                first_edit.get_or_insert(index);
            }
        }
        drop(generation);
        let moved = mosaica_types::by_shard(pairs);
        let carried: Vec<WalRecord> = self.live.with_artifacts(|store| {
            moved
                .iter()
                .flat_map(|(shard, moved)| store.carried_over(*shard, moved))
                .collect()
        });

        mark = self.health.lap(WriteStage::DeriveRecords, mark);

        let shard_records = closing.placing.records();
        let entries_at = vocabulary_records.len() + shard_records.len();
        let artifacts_at = entries_at + closing.entries().len();
        let growth_at = artifacts_at + mint_records.len();
        let carried_at = growth_at + growth.len();
        let durable: Vec<&WalRecord> = vocabulary_records
            .iter()
            .chain(shard_records.iter())
            .chain(closing.entries().iter().map(|entry| &entry.record))
            .chain(mint_records.iter())
            .chain(growth.iter().map(|(record, _)| record))
            .chain(carried.iter())
            .collect();

        let positions = match self.append_and_sync(&durable, Some(mark)) {
            Ok(positions) => positions,
            Err(failure) => {
                // A mint, a publication, or a sync all belong to the window, not to one entry.
                let blamed = match failure {
                    Undurable::Append { at, .. } if (entries_at..artifacts_at).contains(&at) => {
                        at - entries_at
                    }
                    Undurable::Append { at, .. } if at >= carried_at => first_edit.unwrap_or(0),
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
        let sealed = closing.placing.commit(&self.live);
        self.health.record_fragmentation(tally);
        // A sealed shard gets one closing compaction.
        let window_end = *positions.last().expect("a window appends its entries");
        self.compactions.sealed(sealed, window_end);
        if let Some(opened) = &opened {
            self.adopt_opened(opened);
        }
        self.apply_window(
            closing.entries_mut(),
            &positions[entries_at..artifacts_at],
            vocabularies,
            &fresh_bindings,
            opened,
        );

        // After the rows are in force, never before: a membership is projected through rows.
        let artifact_records: Vec<&WalRecord> = mint_records
            .iter()
            .chain(growth.iter().map(|(record, _)| record))
            .chain(carried.iter())
            .collect();
        self.apply_artifact_records(
            &artifact_records,
            &positions[artifacts_at..],
            Publish::AtTick,
        );

        self.record_accepted_batches(closing.entries(), &positions[entries_at..artifacts_at]);
        log_minted_artifacts(&minted_per_entry, &mint_records);
        closing.ack(&self.health, minted_per_entry, joined_per_entry);
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
            self.live.record_accepted_batch(
                batch_id.to_string(),
                body_hash,
                entry.receipt().to_vec(),
                *wal_pos,
            );
        }
        self.health.lap(WriteStage::RecordBatch, m);
        self.observe_wal();
    }

    /// The shards `ids` a window opens. Each is written as a build with no items writes a shard,
    /// with an empty member pack beside each record pack the publications name, and opened over
    /// the live bundle. `None` where `ids` is empty.
    pub(super) fn open_shards(
        &self,
        ids: &[ShardId],
    ) -> Result<Option<OpenedShards>, crate::EngineError> {
        if ids.is_empty() {
            return Ok(None);
        }
        let generation = self.generation.load_full();
        let prefix_dir = self.prefix_dir(&generation);
        let (phash, _) = generation
            .bundle
            .partitions
            .iter()
            .next()
            .expect("a served bundle has a partition");
        let record_packs = self
            .side_manifests
            .spaces
            .iter()
            .flat_map(|(space, held)| {
                held.record_packs
                    .iter()
                    .map(move |extent| (*space, extent.clone()))
            })
            .collect();
        let attributes = self.live.attributes_for_publication();
        let entity: Vec<String> = attributes.0.iter().map(|a| a.name.clone()).collect();
        let mut bundle = mosaica_store::read::Bundle {
            manifest: generation.bundle.manifest.clone(),
            partitions: generation.bundle.partitions.clone(),
        };
        let opened = crate::write::open_shards(
            &crate::write::Opening {
                bundle_root: &self.deps.bundle_root,
                prefix_dir: &prefix_dir,
                prefix: &generation.prefix,
                phash,
                record_packs,
                runtime: crate::write::runtime_columns(attributes),
            },
            &mut bundle,
            ids,
        )?;
        let bundle = Arc::new(bundle);
        let mut members = Vec::with_capacity(opened.len());
        let mut member_files = std::collections::BTreeMap::new();
        for shard in opened {
            members.push((shard.id, shard.extents));
            member_files.extend(shard.files);
        }
        let parts = ids
            .iter()
            .map(|&id| {
                let parts = crate::engine::opened_shard_parts(
                    &prefix_dir,
                    &bundle,
                    phash,
                    id,
                    &entity,
                    generation.segments_version,
                )?;
                Ok((id, parts))
            })
            .collect::<Result<_, crate::EngineError>>()?;
        Ok(Some(OpenedShards {
            bundle,
            parts,
            members,
            member_files,
        }))
    }

    /// Hold what the shards a committed window opened need beside their parts: a slice of every
    /// membership, and their member packs, which the next publication names.
    fn adopt_opened(&mut self, opened: &OpenedShards) {
        self.live
            .open_slices(opened.parts.iter().map(|(shard, _)| *shard));
        for (shard, extents) in &opened.members {
            self.side_manifests
                .membership_extents
                .insert(*shard, extents.clone());
        }
        self.side_manifests
            .artifact_files
            .extend(opened.member_files.clone());
        self.side_manifests.behind_live = true;
    }

    /// Clone each touched shard's buffer once, insert every entry in the window, publish once over
    /// every shard the window writes in and every shard it opened. A clone is O(the shard's
    /// buffered items), so a window of k entries pays it once. `vocabularies` is `close_window`'s
    /// own mutated copy, published verbatim, not cloned from the published `Arc`.
    ///
    /// An edit deletes its old entity and gives the new one the old one's suppression in the same
    /// swap that buffers the new one's rows, so the item is hidden from its acknowledgement until
    /// the new rows flush, and nobody who loses access by the edit keeps it past the ack.
    pub(super) fn apply_window(
        &self,
        closed: &mut [ClosedEntry<Reply<Ingested>>],
        positions: &[u64],
        vocabularies: Vocabularies,
        mints: &[(String, String, u32)],
        opened: Option<OpenedShards>,
    ) {
        let started = std::time::Instant::now();
        let mut mark = StageMark::now();
        let generation = match opened {
            None => self.generation.load_full(),
            Some(opened) => Arc::new(
                self.generation
                    .load()
                    .with_new_shards(opened.bundle, opened.parts),
            ),
        };
        // Grown by exactly the keys this window minted; every other index carries forward.
        let suggest = generation.suggest.with_mints(
            &mosaica_analyse::SuggestionFold::new(),
            &vocabularies,
            mints,
        );
        // A window buffering nothing still publishes its bindings through the last shard.
        let mut written: std::collections::BTreeSet<ShardId> = closed
            .iter()
            .flat_map(|entry| {
                entry
                    .rows()
                    .iter()
                    .map(|row| row.shard)
                    .chain(entry.edits().iter().map(|edit| edit.shard))
            })
            .collect();
        if written.is_empty() {
            written.insert(self.live.last_shard().id);
        }
        let mut touched: std::collections::BTreeMap<ShardId, ShardWrites> = written
            .into_iter()
            .map(|id| {
                let buffer = (*generation.shard_of(id).buffer).clone();
                (
                    id,
                    ShardWrites {
                        buffer,
                        ..Default::default()
                    },
                )
            })
            .collect();
        mark = self.health.lap(WriteStage::ApplyBufferClone, mark);
        let declared = &generation.bundle.manifest.declared_scalars;
        let unique = !generation.unique_live.is_empty_schema();

        for (entry, wal_pos) in closed.iter_mut().zip(positions) {
            let terms = std::mem::take(&mut entry.terms);
            let edit_terms = std::mem::take(&mut entry.edit_terms);
            let rows = entry.rows().iter().zip(terms);
            let edit_rows = entry
                .edits()
                .iter()
                .zip(edit_terms)
                .flat_map(|(edit, terms)| {
                    let mut terms = Some(terms);
                    edit.rows
                        .iter()
                        .map(move |row| (row, terms.take().unwrap_or_default()))
                });
            for (row, row_terms) in rows.chain(edit_rows) {
                let writes = touched
                    .get_mut(&row.shard)
                    .expect("every shard a row names is touched");
                let m = StageMark::now();
                writes.buffer.insert_row_with_terms(row, row_terms);
                let m = self.health.lap(WriteStage::RowBufferInsert, m);
                writes
                    .buffer
                    .set_wal_pos(row.entity_id, &row.view, *wal_pos);
                self.health.lap(WriteStage::RowWalPos, m);
                writes.inserted.push(row.entity_id);
            }
            for edit in entry.edits() {
                let new = edit.rows[0].entity_id;
                let shard = generation.shard_of(edit.shard);
                let writes = touched
                    .get_mut(&edit.shard)
                    .expect("every shard an edit names is touched");
                let overlay = writes
                    .overlay
                    .get_or_insert_with(|| (*shard.overlay).clone());
                overlay.apply(edit.old, mosaica_lifecycle::ChangeOp::Delete);
                writes.deleted.push(edit.old);
                writes.newly_denied.push(edit.old);
                if edit.suppressed {
                    overlay.apply(new, mosaica_lifecycle::ChangeOp::Suppress);
                    writes.newly_denied.push(new);
                }
                writes.buffer.remove(edit.old);
                writes
                    .pairs
                    .push((edit.number.raw() as u32, new.raw() as u32));
            }
        }
        // A joining row's values are its item's, whose entries are held already. An edited item's
        // old entity names nothing, so its entries go.
        let unique_live = unique.then(|| {
            let mut unique_live = (*generation.unique_live).clone();
            for (id, writes) in &touched {
                if !writes.deleted.is_empty() {
                    unique_live.remove_entities(
                        *id,
                        &writes.deleted.iter().map(|e| e.raw() as u32).collect(),
                    );
                }
            }
            unique_live.add(
                declared,
                closed
                    .iter()
                    .flat_map(|entry| {
                        entry
                            .rows()
                            .iter()
                            .chain(entry.edits().iter().flat_map(|edit| &edit.rows))
                    })
                    .filter(|row| !row.join)
                    .map(|row| {
                        (
                            ItemAt::new(row.shard, row.entity_id),
                            row.scalars.as_slice(),
                        )
                    }),
            );
            unique_live
        });
        // An old entity's pair stays until the fold that removes the entity, which reads it to tell
        // an entity an edit made from an item's number.
        let edited_live: std::collections::BTreeMap<ShardId, crate::edited::EditedLive> = touched
            .iter()
            .filter(|(_, writes)| !writes.pairs.is_empty())
            .map(|(id, writes)| {
                let mut edited_live = (*generation.shard_of(*id).edited_live).clone();
                edited_live.add(&writes.pairs);
                (*id, edited_live)
            })
            .collect();
        mark = self.health.lap(WriteStage::ApplyRows, mark);

        let ids: Vec<ShardId> = touched.keys().copied().collect();
        let edited = !edited_live.is_empty();
        let change = |g: &mut crate::GenerationParts, shards: &mut crate::ShardsParts| {
            g.vocabularies = Arc::new(vocabularies);
            g.suggest = suggest;
            if let Some(unique_live) = unique_live {
                g.unique_live = Arc::new(unique_live);
            }
            if edited {
                g.edit_epoch = generation.edit_epoch + 1;
            }
            for id in ids {
                shards.of(id).overlay_version += 1;
            }
            for (id, edited_live) in edited_live {
                shards.of(id).edited_live = Arc::new(edited_live);
            }
        };
        let swaps = touched
            .iter_mut()
            .map(|(id, writes)| {
                let buffer = crate::BufferSwap {
                    buffer: Arc::new(std::mem::take(&mut writes.buffer)),
                    inserted: &writes.inserted,
                };
                let swap = match writes.overlay.take() {
                    None => crate::ShardSwap::Buffer(buffer),
                    Some(overlay) => crate::ShardSwap::Denies(crate::DenySwap {
                        overlay: Arc::new(overlay),
                        newly_denied: &writes.newly_denied,
                        buffer,
                    }),
                };
                (*id, swap)
            })
            .collect();
        let next = generation.publish(swaps, change);
        // Published here, and everywhere else buffer occupancy changes.
        self.health
            .buffered_items
            .store(next.buffered(), Ordering::SeqCst);
        self.publish(next, started);
        self.health.lap(WriteStage::ApplySwap, mark);
    }
}

/// What a window writes in one shard before its publication.
#[derive(Default)]
struct ShardWrites {
    buffer: mosaica_lifecycle::IngestBuffer,
    /// Every entity the window buffered a row for.
    inserted: Vec<EntityId>,
    /// The shard's overlay with the window's edits' denies, where it edits any item.
    overlay: Option<mosaica_lifecycle::Overlay>,
    deleted: Vec<EntityId>,
    newly_denied: Vec<EntityId>,
    /// Each edit's `(number, new entity)`.
    pairs: Vec<(u32, u32)>,
}
