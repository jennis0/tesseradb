//! Bringing every held form of a level forward at a publication, a flush and a merge.

use std::sync::Arc;


use tessera_lifecycle::membership::ArtifactStore;

use tessera_store::permutation::RowSpace;


use super::*;

impl ArtifactProjections {
    /// **Publish the row form of one `(view, layer, level)` from the deltas accumulated since the
    /// last publication** — the flush tick's work, and the only moment a served form changes
    /// (`ingest.md` §1.3, §10 ruling 6).
    ///
    /// A growth, a page, a publication or a fill moves the level's version, and a form rebuilt at
    /// each of them costs 94 to 177 s at rung 3, inside a request, against a 60 s stream deadline
    /// (`2026-09-03-post-flush-artifact-frames.md`). The deltas that produced the moves are small
    /// and this is where they are known, so the interval's are applied together and the key is
    /// moved with them. A request builds no form: one whose deltas are not yet published is served
    /// as last published, up to a tick stale, and a member not yet in an operator is not counted,
    /// so a count can only understate ([`Self::get_or_build`]).
    ///
    /// **Three arms, priced in the line this logs** (`ingest.md` §4.1). A membership join and a
    /// generating-set page of joins alone are unioned into the served operator, which is the same
    /// set the projection would have produced. A page holding any leave re-derives that
    /// `(artifact, view)` operator whole from entity truth, because a union cannot express a leave
    /// and a cardinality moved down against an operator that still holds the leaver is a pair that
    /// was never derived together. A fill re-derives the ordinal's records entry and its operators,
    /// the membership being untouched by one.
    ///
    /// **Every arm takes the delta for this view's own artifacts alone.** One interval's deltas
    /// are applied to every view of the generation, and on a group-scoped layer an ordinal belongs
    /// to one of them (`views.md` §3.5), so each arm reads the store through [`drawn_record`] and
    /// an ordinal this view does not draw is not amended into its form, its records, its column or
    /// its tile index. Projecting a whole level reaches the same rule through
    /// [`tessera_lifecycle::membership::ArtifactStore::level_in_view`].
    ///
    /// **The stored cardinality is published with the operator it was derived with.** Every
    /// ordinal a page or a fill touched has its declared sizes read from the store here, in the
    /// same pass that writes its operators, so the pair a containment test reads is the pair one
    /// moment produced (**I3**; `ingest.md` §1.1, §6.2).
    ///
    /// **A level whose delta moved a generating set gives up its containment partition.** The
    /// partition answers *does this principal's terms reach every member of G* from an expression
    /// composed at a level version; a set that has since grown makes that answer one about a
    /// smaller set, which passes for a principal who does not hold the new member. The level then
    /// serves containment on the masked-count route, which asks `M_auth` itself and is exact.
    ///
    /// **Derived from the level's own records and the view's row space, both authoritative, and
    /// held per `(view, layer, level)`** — nothing per principal, exactly as a built form is
    /// (**I2**). **I11** is kept by what this does *not* touch: no persisted structure is amended,
    /// and the tile index and column derived here are derived rather than re-adopted, because the
    /// fold's files describe the level as it was.
    ///
    /// **A form at a version no delta here follows has missed a write this cannot reconstruct**,
    /// so it is dropped and the next request builds — said at `warn` because it is the expensive
    /// path returning rather than a fault. A form already at or beyond the last delta's version
    /// was built from the store after those writes and is left where it is.
    ///
    /// Nothing is held for most `(view, layer, level)` triples and this then does nothing, which is
    /// the ordinary case for a layer no request has reached.
    #[allow(clippy::too_many_arguments)]
    pub fn publish(
        &self,
        prefix: &str,
        view: &str,
        layer: &str,
        level: u32,
        store: &ArtifactStore,
        space: &RowSpace,
        deltas: &[LevelDelta],
        source: Option<&DeltaRows<'_>>,
    ) {
        // **An attribute predicate has no delta to take.** Its members are the rows carrying a
        // value, which is not in the record this delta came from, so applying one would add
        // nothing and — because it also moves the key — would make the form *hit* on the next
        // request over a value column the geometry has since moved. Such a level is left alone and
        // rebuilt by its own version move, which is what it has always been.
        //
        // **Asked of the declaration and not of [`ProjectionKey::live`]**, which is the segments
        // version and is `0` on a bundle nothing has flushed — indistinguishable there from the `0`
        // a stored level is filed under.
        let Some(source) = source else {
            return;
        };
        let address = (view.to_string(), layer.to_string(), level);
        let Some((Held { key, at, mut rows }, pending, now)) =
            self.taken_to_publish(&address, prefix, space, deltas)
        else {
            return;
        };

        // **A copy only where a request is still reading this form** — the entry was taken from
        // the map above, so between requests this thread is the `Arc`'s only holder and `make_mut`
        // copies nothing. Where a reader does hold it, the copy is the memberships' pointers
        // (`MembershipRows` holds one `Arc` per bitmap) plus the records, generating sets and tile
        // index whole, and `cloned_ms` below is what that cost.
        let started = std::time::Instant::now();
        let shared = Arc::strong_count(&rows) > 1;
        let amended = Arc::make_mut(&mut rows);
        // **The copy alone.** Every arm below is timed by `elapsed_ms`; this is what a concurrent
        // reader cost, and nothing else is inside it.
        let cloned_ms = started.elapsed().as_millis() as u64;
        let applied = Self::applied(amended, &address, store, space, &pending, source);
        let lost = amended.amend_derived(&applied.added, total_rows(space));
        amended.covering(space);
        // **What the interval cost the executor thread**, which is the whole point of applying
        // deltas rather than projecting the level: `cloned_ms` is the copy a concurrent reader
        // forces (see above), `elapsed_ms` the amendment and the tile index beside it. Operator
        // plane only — counts and durations, naming no artifact and no principal.
        tracing::info!(
            layer = %layer,
            level,
            view = %view,
            deltas = pending.len(),
            unions = applied.unions,
            rederived = applied.rederived,
            published = applied.published,
            rows_added = applied.added.len(),
            cloned = shared,
            cloned_ms,
            elapsed_ms = started.elapsed().as_millis() as u64,
            "a level's row forms are published from the deltas since the last tick"
        );
        // The entry is already out of the map, so a failure here drops it rather than putting it
        // back.
        if lost && !self.kept_without_a_column(&address, &mut rows, &applied.added, "amended") {
            return;
        }
        let amended = Arc::make_mut(&mut rows);
        // **A publication carries the containment partition over; a page of a generating set takes
        // it away.** The partition is composed from the level's records at a version and answers
        // per `(ordinal, rank)`. A membership join changes no set, and a publication only appends
        // ordinals, which `ContainmentAnswers::covers` reports as uncovered and sends to the
        // masked-count route. A page *does* change a set: against a set that has since grown the
        // partition's answer is one about a smaller set, which passes for a principal who does not
        // hold the new member. So the level gives the structure up and serves containment on the
        // masked-count route, which asks `M_auth` itself.
        if applied.sets_moved && amended.partition.is_some() {
            amended.partition = None;
            tracing::info!(
                layer = %layer,
                level,
                view = %view,
                "a generating set moved, so this level's containment partition is dropped and \
                 containment is answered from the mask"
            );
        }
        let key = ProjectionKey {
            level_version: now,
            ..key
        };
        // **Filed rather than offered.** This runs on the executor, which holds the newest of both
        // versions: the form came out of the map a moment ago at the live row space, and the
        // deltas are every write the store has taken. A build that straddled this publication
        // describes fewer records over no newer a row space, so there is nothing here for
        // [`Self::insert_newest`] to protect.
        self.cached
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .insert(address, Held { key, at, rows });
    }

    /// **The form this publication amends, taken out of the map with the deltas it has not yet
    /// taken** — `None` where nothing is held, where there is nothing to apply, and where what is
    /// held describes something other than what these deltas follow, each of which this disposes of
    /// itself.
    ///
    /// **Read under the lock first, and taken out of the map only where there is an amendment
    /// to make.** A request arriving while the entry is out finds nothing held and projects the
    /// level whole, on the request path — the cost this whole mechanism exists to avoid — so
    /// the window is narrowed to the amendment itself (`elapsed_ms` in the line below, 15 ms at
    /// rung 3's `mesh/descriptors`) and a level with no delta to take never leaves the map at
    /// all.
    ///
    /// **Taking it is what makes the amendment cheap**: between requests the caller is then
    /// the `Arc`'s only holder, so `Arc::make_mut` copies nothing. Cloning the entry instead
    /// would leave the map holding a second reference and copy the level's records, generating
    /// sets and tile index on every publication.
    fn taken_to_publish<'d>(
        &self,
        address: &LevelAddress,
        prefix: &str,
        space: &RowSpace,
        deltas: &'d [LevelDelta],
    ) -> Option<(Held, Vec<&'d LevelDelta>, u64)> {
        let (view, layer, level) = address;
        let (read_prefix, read_at, pending_from) = {
            let cached = self.cached.lock().unwrap_or_else(|e| e.into_inner());
            let held = cached.get(address)?;
            (held.key.prefix.clone(), held.at, held.key.level_version)
        };
        // **The deltas this form has not taken**, which is those at or after the version it
        // stands at. The interval's deltas carry consecutive versions — every route that changes a
        // level's records bumps it exactly once (`ArtifactStore::bump`'s callers) — so a form at
        // version *v* is completed by the run beginning at *v*, and a form built from the store
        // mid-interval takes only what landed after its build.
        let pending: Vec<&LevelDelta> = deltas
            .iter()
            .filter(|delta| delta.before >= pending_from)
            .collect();
        let now = pending
            .last()
            .map_or(pending_from, |delta| delta.before + 1);
        if pending.is_empty() && read_prefix == prefix {
            // The form was built from the store after every delta held here, and it is still the
            // form this prefix serves. Nothing to apply, and it never left the map.
            return None;
        }
        let Some(Held { key, at, rows }) = self
            .cached
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .remove(address)
        else {
            // A request replaced or a drop removed the entry between the two locks. Whatever
            // stands there now was filed against the store this delta has already reached.
            return None;
        };
        if key.level_version != pending_from || at != read_at {
            // The same race one step in: the entry that came out is not the one that was read, so
            // the run of deltas selected above may not be its own. It goes back untouched and the
            // next tick publishes onto it.
            self.cached
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .insert(address.clone(), Held { key, at, rows });
            return None;
        }
        // **A form that borrowed is rebuilt rather than amended** ([`Self::drop_borrowed`]). The
        // entry is already out of the map, so returning here is what drops it.
        if !rows.inherited.is_empty() {
            drop(rows);
            self.drop_borrowed(address, view);
            return None;
        }
        // **Every term that would make the amendment describe something other than what is held.**
        // A form from another prefix or at a version no delta follows has missed a write; a form
        // whose rows are not rows of this row space is the merge case [`ArtifactRows::covers`]
        // argues. Each is a form that *should* have been published and was not, which is why each
        // is said at `warn`: it is the whole-level projection returning to the request path.
        let reason = if key.prefix != prefix {
            Some("the form was projected under another prefix")
        } else if pending
            .first()
            .is_some_and(|delta| delta.before != key.level_version)
        {
            Some("the form is at a level version these deltas do not follow")
        } else if !(rows.covers(space) && rows.extends_to(space)) {
            // **Exactly this row space, not merely one it can answer for.** The amendment sizes
            // the tile index and the column to `space`, so a form holding rows above what `space`
            // addresses would have them dropped rather than kept. The executor holds the newest
            // generation, so what reaches this arm is a form a request built against a superseded
            // generation and inserted afterwards — see [`ArtifactRows::covers`].
            Some("the form's rows are not rows of this view's row space")
        } else {
            None
        };
        if let Some(reason) = reason {
            // The entry is already out of the map; dropping `rows` here is what drops the form.
            tracing::warn!(
                layer = %layer,
                level,
                view = %view,
                reason,
                "a level's held row form could not be published and is dropped; the next \
                 request naming this level projects it whole"
            );
            return None;
        }
        Some((Held { key, at, rows }, pending, now))
    }

    /// **Every pending delta applied to the form**, and the operators a page or a fill moved read
    /// again from the store beside them.
    ///
    /// **The operator and the cardinality beside it, from one read of the store.** A
    /// re-derivation projects the artifact's sets again; a refresh alone takes the declared
    /// sizes the pages moved. Both read the record as it stands now, which is what makes the
    /// pair a containment test reads a pair one moment produced.
    fn applied(
        amended: &mut ArtifactRows,
        address: &LevelAddress,
        store: &ArtifactStore,
        space: &RowSpace,
        pending: &[&LevelDelta],
        source: &DeltaRows<'_>,
    ) -> Applied {
        let (view, layer, level) = address;
        let level = *level;
        let row_major = amended.layout.is_row_major();
        let mut applied = Applied::default();
        // Ordinals whose records entry is read again — a fill's parts, and the stored cardinality
        // a page moved — and those whose operators are re-derived from entity truth.
        let mut refresh: std::collections::BTreeSet<u32> = std::collections::BTreeSet::new();
        let mut rederive: std::collections::BTreeSet<u32> = std::collections::BTreeSet::new();
        for delta in pending {
            match &delta.kind {
                DeltaKind::Grown { joins, pages } => {
                    for (ordinal, joining) in joins {
                        if drawn_record(store, layer, level, *ordinal, view).is_none() {
                            continue;
                        }
                        let fresh = amended.grow_rows(*ordinal, joining, space);
                        applied.unions += 1;
                        if row_major {
                            applied.added.extend(fresh.iter().map(|row| (row, *ordinal)));
                        }
                    }
                    for page in pages {
                        if drawn_record(store, layer, level, page.ordinal, view).is_none() {
                            continue;
                        }
                        applied.sets_moved = true;
                        refresh.insert(page.ordinal);
                        if page.whole {
                            rederive.insert(page.ordinal);
                            continue;
                        }
                        // A join alone: the same set the projection would have produced, reached
                        // by one union over the page's own members.
                        if amended.grow_generating(page.ordinal, page.rank, &page.joining, space) {
                            applied.unions += 1;
                        }
                    }
                }
                DeltaKind::Filled(ordinals) => {
                    for ordinal in ordinals {
                        match source {
                            DeltaRows::Projected => {
                                refresh.insert(*ordinal);
                                rederive.insert(*ordinal);
                            }
                            // **A spatial level's membership is its shape**, so a fill that
                            // supplied one gives the artifact rows it did not have. The ordinal is
                            // re-placed from the resolution the caller made over every live
                            // segment, which is the arm a publication into such a level takes.
                            DeltaRows::Resolved(rows) => {
                                if let Some(record) =
                                    drawn_record(store, layer, level, *ordinal, view)
                                {
                                    applied.published += 1;
                                    let fresh = amended.publish_resolved(
                                        *ordinal,
                                        record,
                                        rows(*ordinal),
                                        space,
                                    );
                                    if row_major {
                                        applied.added.extend(fresh.iter().map(|row| (row, *ordinal)));
                                    }
                                }
                            }
                        }
                    }
                }
                DeltaKind::Published(ordinals) => {
                    for ordinal in ordinals {
                        if let Some(record) = drawn_record(store, layer, level, *ordinal, view) {
                            applied.published += 1;
                            let fresh = match source {
                                DeltaRows::Projected => amended.publish_at(*ordinal, record, space),
                                DeltaRows::Resolved(rows) => amended.publish_resolved(
                                    *ordinal,
                                    record,
                                    rows(*ordinal),
                                    space,
                                ),
                            };
                            if row_major {
                                applied.added.extend(fresh.iter().map(|row| (row, *ordinal)));
                            }
                        }
                    }
                }
            }
        }
        for ordinal in &refresh {
            if let Some(record) = drawn_record(store, layer, level, *ordinal, view) {
                amended.refresh_sets(*ordinal, record, space, rederive.contains(ordinal));
            }
        }
        applied.rederived = rederive.len();
        applied
    }

    /// **Extend every stored level's held form in one view by the segment a flush has published.**
    ///
    /// A form's memberships cover the whole row space (`MembershipRows::put`), so a flush that
    /// appends an extent leaves every one of them one segment short: an ingested row that joined an
    /// artifact would count for nobody until the next fold. This is where the segment reaches them
    /// — one `project_extents_from` per artifact, over the entities inside that extent's own range.
    ///
    /// `rows` says per `(layer, level)` where the segment's rows come from — projected from a
    /// stored membership's records, or the segment's resolution against a spatial level's shapes
    /// ([`SegmentRows`]) — and `None` for an attribute predicate, whose form is keyed on the
    /// geometry and rebuilt by the flush's own move of it. Asked of the declaration and not of
    /// [`ProjectionKey::live`], for [`Self::bring_forward`]'s reason.
    ///
    /// `previous` is the row space the outgoing generation served and `next` the one being
    /// published; `at` is the segments version `next` carries. A form that agreed with `previous`
    /// extends to `next`, because a flush appends and nothing else moved between the two — asserted
    /// in a debug build. A form that did not agree with `previous` was built by a request against a
    /// generation superseded while it built ([`ArtifactRows::covers`]); it is dropped with a `warn`,
    /// and the next request naming the level projects it.
    #[allow(clippy::too_many_arguments)]
    pub fn extend_flushed(
        &self,
        prefix: &str,
        view: &str,
        store: &ArtifactStore,
        previous: &RowSpace,
        next: &RowSpace,
        at: u64,
        rows_of: &dyn Fn(&str, u32) -> Option<SegmentRows>,
    ) {
        for (address, key, mut rows) in self.held_of_view(prefix, view) {
            let (_, layer, level) = &address;
            if !rows.inherited.is_empty() {
                // [`Self::drop_borrowed`]: the rows are not this level's records' to extend.
                self.drop_borrowed(&address, view);
                continue;
            }
            if rows.covers(next) {
                continue;
            }
            if !rows.extends_to(next) {
                debug_assert!(
                    !rows.agrees_with(previous),
                    "a held row form agreed with the outgoing generation and does not extend to \
                     the flushed one: a publication permuted rows without rebasing the form"
                );
                self.drop_disagreeing(&address, view);
                continue;
            }
            let Some(source) = rows_of(layer, *level) else {
                continue;
            };
            let started = std::time::Instant::now();
            let amended = Arc::make_mut(&mut rows);
            let (added, rows_taken) = match source {
                SegmentRows::Projected => {
                    amended.extend_by(store.level_in_view(layer, *level, view_key(view)), next)
                }
                SegmentRows::Resolved(piece) => {
                    // The one segment resolved is the one this flush published, so a form more
                    // than one segment short has nothing here for the others — the straddling
                    // build's form again, dropped for the same reason.
                    let Some(extent) = next
                        .extents()
                        .last()
                        .filter(|_| amended.covered.len() + 1 == next.extent_count())
                    else {
                        self.drop_disagreeing(&address, view);
                        continue;
                    };
                    // The memberships come from the resolution; the generating sets are entity
                    // sets and are projected from the records either way.
                    amended.extend_by_resolved(
                        &piece,
                        store.level_in_view(layer, *level, view_key(view)),
                        next,
                        extent.row_base,
                    )
                }
            };
            let lost = amended.amend_derived(&added, total_rows(next));
            amended.covering(next);
            // **What the flush cost the executor thread**, beside the merge's line: the rows the
            // segment put into memberships, over every artifact, and the column labels among
            // them. Operator plane only.
            tracing::info!(
                layer = %layer,
                level,
                view = %view,
                rows_taken,
                labels_added = added.len(),
                elapsed_ms = started.elapsed().as_millis() as u64,
                "a level's held row form took a flush's segment"
            );
            if lost && !self.kept_without_a_column(&address, &mut rows, &added, "extended") {
                continue;
            }
            self.insert_newest(address, Held { key, at, rows });
        }
    }

    /// **Rebase every stored level's held form in one view over the extent a merge has published**
    /// — the one geometry publication that permutes rows a form holds rather than appending to
    /// them, taken in place before the swap exactly as a flush's extension is.
    ///
    /// The merged extent `merged` stands at the index the consumed run's first segment stood at,
    /// with the same `row_base` and row count, so what changes for a form is the bits inside that
    /// span and the segment list it records. [`ArtifactRows::rebase_span`] clears the span and
    /// re-projects each artifact's members through the merged extent — or, for a spatial level,
    /// puts the merged segment's resolution there ([`SegmentRows::Resolved`]); the column gives up
    /// the span's labels and takes the new ones ([`RowColumn::rebase`](crate::row_column::RowColumn::rebase)); the tile index is derived
    /// again; and the form's record of its segments becomes `next`'s. The containment partition is
    /// untouched, being per ordinal and rank and not per row.
    ///
    /// Without this the form failed [`ArtifactRows::covers`] on the next request and the level
    /// was projected whole inside it — 108 s at rung 3's `mesh/descriptors`, shed at the 60 s
    /// stream deadline, once per merge (`probes/2026-09-05-merge-arm/`).
    ///
    /// `previous`, `next`, `at` and the disposition of a form that did not agree with `previous`
    /// are [`Self::extend_flushed`]'s, and so is a form that agrees with `previous` but stops
    /// short of it: a build against an older generation inserted where nothing stood, which the
    /// flushes between had nothing to extend. A form that covers `previous` exactly is rebased;
    /// the executor holds the newest generation, so no held form is longer.
    #[allow(clippy::too_many_arguments)]
    pub fn rebase_merged(
        &self,
        prefix: &str,
        view: &str,
        store: &ArtifactStore,
        previous: &RowSpace,
        next: &RowSpace,
        merged: &str,
        at: u64,
        rows_of: &dyn Fn(&str, u32) -> Option<SegmentRows>,
    ) {
        let Some(start) = next
            .extents()
            .iter()
            .position(|extent| extent.seg_id == merged)
        else {
            return;
        };
        for (address, key, mut rows) in self.held_of_view(prefix, view) {
            let (_, layer, level) = &address;
            if !rows.inherited.is_empty() {
                // [`Self::drop_borrowed`]: the rows are not this level's records' to rebase.
                self.drop_borrowed(&address, view);
                continue;
            }
            if !(rows.covers(previous) && rows.extends_to(previous)) {
                // A form that agrees with `previous` and is shorter than it is a straddling
                // build's: built against an older generation and inserted where nothing stood,
                // after the flushes between had nothing to extend. It is dropped with the others.
                // What cannot happen is a form that agrees, covers `previous` whole and does not
                // equal it: no held form is longer than the newest generation.
                debug_assert!(
                    !rows.agrees_with(previous) || rows.covered.len() < previous.extent_count(),
                    "a held row form agreed with the outgoing generation and covered more of it \
                     than the generation has"
                );
                self.drop_disagreeing(&address, view);
                continue;
            }
            let Some(source) = rows_of(layer, *level) else {
                continue;
            };
            let started = std::time::Instant::now();
            let amended = Arc::make_mut(&mut rows);
            let (lo, hi, added, rows_taken) = match source {
                SegmentRows::Projected => amended.rebase_span(
                    store.level_in_view(layer, *level, view_key(view)),
                    next,
                    start,
                ),
                SegmentRows::Resolved(piece) => amended.rebase_span_resolved(
                    &piece,
                    store.level_in_view(layer, *level, view_key(view)),
                    next,
                    start,
                ),
            };
            let lost = amended.rebase_derived(lo, hi, &added, total_rows(next));
            amended.covering(next);
            tracing::info!(
                layer = %layer,
                level,
                view = %view,
                seg_id = %merged,
                span_rows = hi - lo,
                rows_taken,
                labels_added = added.len(),
                elapsed_ms = started.elapsed().as_millis() as u64,
                "a level's held row form took a merge's rebase"
            );
            if lost && !self.kept_without_a_column(&address, &mut rows, &added, "rebased") {
                continue;
            }
            self.insert_newest(address, Held { key, at, rows });
        }
    }

    /// **What a level does when its memberships no longer partition** — the disposition all three
    /// amendments share, `amendment` naming which one reached it.
    ///
    /// A form that holds its own bitmaps goes back to the artifact-major route, which answers
    /// identically. **A form that holds no bitmaps takes the list form instead of falling back**:
    /// its column is its membership, so there is nothing to fall back *to*, and the
    /// list form is what a fold would choose for a level that has stopped partitioning
    /// (decision 0094). Composed through the disk-backed partition route from the
    /// column it already holds — nothing row-sized is held.
    ///
    /// `false` where that recomposition failed: the form is dropped here and the caller has
    /// nothing left to file.
    fn kept_without_a_column(
        &self,
        address: &LevelAddress,
        rows: &mut Arc<ArtifactRows>,
        added: &[(u32, u32)],
        amendment: &str,
    ) -> bool {
        let (view, layer, level) = address;
        self.fallbacks
            .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        if rows.membership().rows_held() {
            tracing::warn!(
                layer = %layer,
                level,
                view = %view,
                "this level's {amendment} memberships no longer partition, so it is served \
                 artifact-major. Every answer is unchanged; the layout is not"
            );
            return true;
        }
        let started = std::time::Instant::now();
        let scratch = self.scratch().to_path_buf();
        if !Arc::make_mut(rows).recompose_as_list(added, &scratch) {
            self.drop_lost_column(address, view);
            return false;
        }
        self.columns_composed
            .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        tracing::warn!(
            layer = %layer,
            level,
            view = %view,
            elapsed_ms = started.elapsed().as_millis() as u64,
            "this level's {amendment} memberships no longer partition and it is served from \
             its column alone, so the column is recomposed in the list form. Every answer \
             is unchanged; the layout is not"
        );
        true
    }

    /// Every form held for `view` under `prefix`, cloned out of the map so the amendment runs
    /// outside its lock.
    fn held_of_view(
        &self,
        prefix: &str,
        view: &str,
    ) -> Vec<(LevelAddress, ProjectionKey, Arc<ArtifactRows>)> {
        let cached = self.cached.lock().unwrap_or_else(|e| e.into_inner());
        cached
            .iter()
            .filter(|((held_view, _, _), _)| held_view == view)
            .filter(|(_, held)| held.key.prefix == prefix)
            .map(|(address, held)| (address.clone(), held.key.clone(), Arc::clone(&held.rows)))
            .collect()
    }

    /// Drop a column-only form whose column could not be recomposed in the list form.
    ///
    /// **An I/O failure and not a shape.** A list column expresses any membership, so the
    /// recomposition ([`ArtifactRows::recompose_as_list`]) has no case it cannot represent; what
    /// reaches this is a scratch directory that would not take the composition or a file that
    /// would not read back. The level is projected whole by the next request that names it, which
    /// is what every request did before this form existed.
    fn drop_lost_column(&self, address: &LevelAddress, view: &str) {
        let (_, layer, level) = address;
        self.cached
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .remove(address);
        tracing::warn!(
            layer = %layer,
            level,
            view = %view,
            "ALARM: this level's column could not be recomposed in the list form, so the level \
             has no membership left to serve; the form is dropped and the next request naming \
             this level projects it whole"
        );
    }

    /// Drop a form that borrowed a membership rather than amend it ([`ArtifactRows::inherit`]).
    ///
    /// Every amendment below carries what a level's own records changed by, and a borrowing
    /// artifact's membership is not in its record: a delta hands it the empty set it declared, and
    /// a flush's extension finds nothing of its own to extend. The next request naming the level
    /// rebuilds the form and resolves every borrowed membership against the store as it stands
    /// then. Said at `debug`, because this is the ordinary course for such a level rather than a
    /// fault, and what is given up is a warm form on a level holding one artifact per cluster.
    fn drop_borrowed(&self, address: &LevelAddress, view: &str) {
        let (_, layer, level) = address;
        self.cached
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .remove(address);
        tracing::debug!(
            layer = %layer,
            level,
            view = %view,
            "this level's artifacts borrow their membership from what they attach to, so its held \
             row form is dropped rather than amended; the next request naming it resolves them \
             again"
        );
    }

    /// Drop a form whose rows are not rows of the generation being published — see
    /// [`ArtifactRows::covers`] on the one way such a form is held.
    fn drop_disagreeing(&self, address: &LevelAddress, view: &str) {
        let (_, layer, level) = address;
        self.cached
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .remove(address);
        tracing::warn!(
            layer = %layer,
            level,
            view = %view,
            "a level's held row form was built against a generation superseded while it built and \
             does not agree with the one being published; it is dropped and the next request \
             naming this level projects it whole"
        );
    }
}

/// **What one interval's deltas did to one form** — the three arms of `ingest.md` §4.1, counted for
/// the line [`ArtifactProjections::publish`] logs.
#[derive(Default)]
struct Applied {
    /// **The rows these deltas gave each artifact**, gathered as the membership takes them, so
    /// the column is amended at exactly those and the pack is never rewritten. Empty on an
    /// artifact-major level, which has no column to amend.
    added: Vec<(u32, u32)>,
    /// Sets unioned into the served operator.
    unions: u64,
    /// Ordinals published.
    published: u64,
    /// Operators re-derived whole from entity truth.
    rederived: usize,
    /// Whether a page moved a generating set, which is what takes the containment partition away.
    sets_moved: bool,
}
