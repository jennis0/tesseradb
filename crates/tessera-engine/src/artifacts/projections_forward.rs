//! Bringing every held form of a level forward at a publication, a flush and a merge.

use std::sync::Arc;


use tessera_lifecycle::membership::ArtifactStore;

use tessera_store::permutation::RowSpace;


use super::*;

impl ArtifactProjections {
    /// Publish the row form of one `(view, layer, level)` from the deltas accumulated since the
    /// last publication — the flush tick's work, and the only moment a served form changes.
    ///
    /// A growth, a page, a publication or a fill moves the level's version, and the interval's
    /// deltas are applied together rather than one form rebuild per move. A request builds no
    /// form: one whose deltas are not yet published is served as last published, up to a tick
    /// stale, so a count can only understate ([`Self::get_or_build`]).
    ///
    /// Three arms. A membership join and a generating-set page of joins alone are unioned into the
    /// served operator, the same set a projection would produce. A page holding any leave
    /// re-derives that operator whole from entity truth, because a union cannot express a leave. A
    /// fill re-derives the ordinal's records entry and its operators; the membership is untouched.
    ///
    /// Every arm takes the delta for this view's own artifacts alone, reading the store through
    /// [`drawn_record`], so an ordinal this view does not draw is not amended into its form.
    ///
    /// The stored cardinality is published with the operator it was derived with, in the same pass
    /// that writes the operator, so the pair a containment test reads is the pair one moment
    /// produced. A level whose delta moved a generating set gives up its containment partition: a
    /// set that has since grown makes the partition's answer one about a smaller set, which would
    /// pass a principal who does not hold the new member, so the level serves containment from the
    /// mask instead.
    ///
    /// Derived from the level's own records and the view's row space, both authoritative, and held
    /// per `(view, layer, level)`: nothing per principal. No persisted structure is amended.
    ///
    /// A form at a version no delta here follows has missed a write this cannot reconstruct, so it
    /// is dropped and the next request builds. Nothing is held for most triples and this then does
    /// nothing, the ordinary case for a layer no request has reached.
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
        // An attribute predicate has no delta to take: its members are the rows carrying a value,
        // not in the record this delta came from, so applying one would add nothing and would
        // make the form hit on a value column the geometry has since moved. Such a level is left
        // alone and rebuilt by its own version move.
        let Some(source) = source else {
            return;
        };
        let address = (view.to_string(), layer.to_string(), level);
        let Some((Held { key, at, mut rows }, pending, now)) =
            self.taken_to_publish(&address, prefix, space, deltas)
        else {
            return;
        };

        // A copy only where a request is still reading this form: the entry was taken from the map
        // above, so between requests this thread is the `Arc`'s only holder and `make_mut` copies
        // nothing.
        let started = std::time::Instant::now();
        let shared = Arc::strong_count(&rows) > 1;
        let amended = Arc::make_mut(&mut rows);
        // The copy alone; every arm below is timed by `elapsed_ms`.
        let cloned_ms = started.elapsed().as_millis() as u64;
        let applied = Self::applied(amended, &address, store, space, &pending, source);
        let lost = amended.amend_derived(&applied.added, total_rows(space));
        amended.covering(space);
        // Operator plane only: counts and durations, naming no artifact and no principal.
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
        if lost && !self.kept_without_a_column(&address, &mut rows, &applied.added, "amended") {
            return;
        }
        let amended = Arc::make_mut(&mut rows);
        // See the doc above: a page moving a generating set takes the containment partition away.
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
        // Filed rather than offered: the executor holds the newest generation already.
        self.cached
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .insert(address, Held { key, at, rows });
    }

    /// The form this publication amends, taken out of the map with the deltas it has not yet taken.
    /// `None` where nothing is held, where there is nothing to apply, and where what is held
    /// describes something other than what these deltas follow, each of which this disposes of.
    ///
    /// Read under the lock first, and taken out of the map only where there is an amendment to
    /// make, so a level with no delta to take never leaves the map at all. Taking it, rather than
    /// cloning it, is what makes `Arc::make_mut` below copy nothing between requests.
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
        // The deltas this form has not taken: those at or after the version it stands at. Deltas
        // carry consecutive versions, so a form at version *v* is completed by the run from *v*.
        let pending: Vec<&LevelDelta> = deltas
            .iter()
            .filter(|delta| delta.before >= pending_from)
            .collect();
        let now = pending
            .last()
            .map_or(pending_from, |delta| delta.before + 1);
        if pending.is_empty() && read_prefix == prefix {
            return None;
        }
        let Some(Held { key, at, rows }) = self
            .cached
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .remove(address)
        else {
            // A request replaced or a drop removed the entry between the two locks.
            return None;
        };
        if key.level_version != pending_from || at != read_at {
            // The same race one step in: the run of deltas selected above may not be this entry's
            // own. It goes back untouched and the next tick publishes onto it.
            self.cached
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .insert(address.clone(), Held { key, at, rows });
            return None;
        }
        // A form that borrowed is rebuilt rather than amended ([`Self::drop_borrowed`]).
        if !rows.inherited.is_empty() {
            drop(rows);
            self.drop_borrowed(address, view);
            return None;
        }
        // A form from another prefix, at a version no delta follows, or the merge case
        // [`ArtifactRows::covers`] argues: each should have been published and was not.
        let reason = if key.prefix != prefix {
            Some("the form was projected under another prefix")
        } else if pending
            .first()
            .is_some_and(|delta| delta.before != key.level_version)
        {
            Some("the form is at a level version these deltas do not follow")
        } else if !(rows.covers(space) && rows.extends_to(space)) {
            Some("the form's rows are not rows of this view's row space")
        } else {
            None
        };
        if let Some(reason) = reason {
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

    /// Every pending delta applied to the form. A re-derivation projects the artifact's sets
    /// again; a refresh alone takes the declared sizes the pages moved.
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
        // Ordinals whose records entry is read again, and those whose operators are re-derived
        // from entity truth.
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
                        // A join alone: the same set a projection would produce, by one union.
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
                            // A spatial level's membership is its shape, so a fill that supplied
                            // one gives the artifact rows it did not have.
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

    /// Extend every stored level's held form in one view by the segment a flush has published.
    ///
    /// A form's memberships cover the whole row space, so a flush that appends an extent leaves
    /// every one of them one segment short: an ingested row that joined an artifact would count
    /// for nobody until the next fold. `rows` says per `(layer, level)` where the segment's rows
    /// come from ([`SegmentRows`]), `None` for an attribute predicate, whose form is rebuilt by
    /// the flush's own move of the geometry.
    ///
    /// `previous` is the row space the outgoing generation served and `next` the one being
    /// published; `at` is the segments version `next` carries. A form that did not agree with
    /// `previous` was built by a request against a generation superseded while it built
    /// ([`ArtifactRows::covers`]) and is dropped.
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
                    // than one segment short has nothing here for the others.
                    let Some(extent) = next
                        .extents()
                        .last()
                        .filter(|_| amended.covered.len() + 1 == next.extent_count())
                    else {
                        self.drop_disagreeing(&address, view);
                        continue;
                    };
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

    /// Rebase every stored level's held form in one view over the extent a merge has published —
    /// the one geometry publication that permutes rows a form holds rather than appending to them.
    ///
    /// The merged extent `merged` stands at the index the consumed run's first segment stood at,
    /// with the same `row_base` and row count, so what changes for a form is the bits inside that
    /// span. [`ArtifactRows::rebase_span`] clears the span and re-projects each artifact's members
    /// through the merged extent, or for a spatial level puts the merged segment's resolution
    /// there ([`SegmentRows::Resolved`]); the column and tile index are rebuilt for the span. The
    /// containment partition is untouched, being per ordinal and rank and not per row.
    ///
    /// `previous`, `next`, `at` and the disposition of a form that did not agree with `previous`
    /// are [`Self::extend_flushed`]'s. A form that covers `previous` exactly is rebased.
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
                self.drop_borrowed(&address, view);
                continue;
            }
            if !(rows.covers(previous) && rows.extends_to(previous)) {
                // A straddling build's form, dropped with the others.
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

    /// What a level does when its memberships no longer partition, `amendment` naming which of the
    /// three amendments reached it. A form holding its own bitmaps goes back to the artifact-major
    /// route; one holding no bitmaps takes the list form instead, since its column is its
    /// membership. `false` where that recomposition failed and the form is dropped.
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

    /// Every form held for `view` under `prefix`, cloned out so the amendment runs outside the lock.
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

    /// Drop a column-only form whose column could not be recomposed in the list form. An I/O
    /// failure and not a shape: a list column expresses any membership, so what reaches this is a
    /// scratch directory that would not take the composition or a file that would not read back.
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

    /// Drop a form that borrowed a membership rather than amend it ([`ArtifactRows::inherit`]): the
    /// next request rebuilds it. Said at `debug`: the ordinary course, not a fault.
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

    /// Drop a form whose rows are not rows of the generation being published ([`ArtifactRows::covers`]).
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

/// What one interval's deltas did to one form, counted for the line [`ArtifactProjections::publish`]
/// logs.
#[derive(Default)]
struct Applied {
    /// The rows these deltas gave each artifact, so the column is amended at exactly those rather
    /// than rewritten. Empty on an artifact-major level, which has no column to amend.
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
