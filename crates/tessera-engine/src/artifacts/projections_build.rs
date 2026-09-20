//! Getting or building one level's row form, and the pieces it claims or composes.

use std::sync::Arc;


use tessera_lifecycle::membership::ArtifactStore;
use tessera_types::layer::ServingLayout;

use tessera_store::permutation::RowSpace;

use crate::containment::{ContainmentPartition, PartitionSource};
use crate::row_column::RowColumn;
use crate::tile_index::TileIndex;

use super::*;

impl ArtifactProjections {
    /// This level's row form for the generation `store` is in, building it if what is held is
    /// stale.
    ///
    /// **The version is read from the store this builds from, never handed in.** Both come from
    /// the one borrow, so the form cached under version *v* is the form of the level *at* version
    /// *v* — there is no window in which a caller's separately-read version could label a form
    /// built from records that have since moved. That is the whole of the freshness argument, and
    /// a stale form here is a wrong masked count with nothing reporting a fault.
    ///
    /// **The returned version is the form's own, which is not always the store's.** The level
    /// version is a floor here ([`ProjectionKey::stale_form_of`]), so between an accepted write and
    /// the tick that publishes its delta the form handed back is the level as last published and
    /// stands at the *earlier* version. A caller keying anything on the store's version instead
    /// would file a derivation of this form under a version it is not of — and the masked-count
    /// histogram, which decides a row-major level's candidacy, would then be read by every later
    /// request in the session as though it had counted the grown column.
    ///
    /// **The build runs outside this cache's lock**, so a slow projection does not block every
    /// other layer's requests behind it. Two threads racing the same key both build and the last
    /// one wins; they build from the same level version over the same row space, so the two
    /// results are equal and the waste is one projection, not a wrong answer.
    // Nine, and every one is a thing a level's derived form is *of*: where it came from (prefix,
    // view, layer, level), what it is built from (the store, the row space), which form it is
    // served in, and what the containment partition needs beside them. Bundling them would name
    // the same nine things one call earlier — the argument `serve_artifacts` already makes for its
    // own.
    #[allow(clippy::too_many_arguments)]
    pub fn get_or_build(
        &self,
        prefix: &str,
        view: &str,
        layer: &str,
        level: u32,
        store: &ArtifactStore,
        space: &RowSpace,
        source: Option<&PartitionSource<'_>>,
        layout: ServingLayout,
        predicate: Option<&PredicateSource<'_>>,
        segments_version: u64,
        column_only: bool,
    ) -> (Arc<ArtifactRows>, u64) {
        let key = ProjectionKey {
            prefix: prefix.to_string(),
            view: view.to_string(),
            level_version: store.level_version(layer, level),
            // See [`ProjectionKey::live`]: a value column is evaluated against the geometry; a
            // stored membership and a spatial one are brought forward with it.
            live: match predicate {
                Some(PredicateSource::Attribute(_)) => segments_version,
                _ => 0,
            },
        };
        let map_key = (view.to_string(), layer.to_string(), level);

        if let Some(held) = self
            .cached
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .get(&map_key)
        {
            // **The key and the row space both**, and neither implies the other. The key says the
            // form describes this level's records; [`ArtifactRows::covers`] says its rows are rows
            // of this row space — which the key cannot, because a flush and a merge move no term
            // of it. See that method for why the row space is not simply a fourth term here: a
            // flush *extends the form* rather than invalidating it, so a form whose rows are one
            // segment short is a form to extend and not one to rebuild.
            //
            // **The level version is a floor and not an equality** (`ingest.md` §1.3, §10 ruling
            // 6). A write moves the version and the form takes the delta at the next tick, so
            // between the two the held form is the level as last published: a request is served
            // that, up to a tick stale, rather than building the level again on the request path.
            // What it costs is a member not yet in an operator, which is not counted — a count
            // understates and never the reverse — and a containment test reads the operator and
            // the cardinality this form published together. Every other term of the key is an
            // equality: a form under another prefix, or of another view, or of an attribute
            // predicate whose value column the geometry has moved, describes something else.
            // **And what it borrowed is still what it borrowed.** A level whose artifacts take
            // their membership from another's is filed under its *own* version, which a target
            // that grew did not move; without this term a label would answer over the membership
            // its cluster had when the form was built (`ArtifactRows::inherited`).
            if held.key.stale_form_of(&key)
                && held.rows.covers(space)
                && held.rows.inherited_current(store)
            {
                // **Its own version and not `key`'s**: see the doc above. A form still waiting for
                // a tick's delta is the level at the earlier version, and that is what anything
                // derived from it must be filed under.
                return (Arc::clone(&held.rows), held.key.level_version);
            }
        }

        // **Both derivations, and the partition, from one borrow of the store at one level
        // version.** The row form, the records and the containment partition describe the same
        // population, and a growth landing between two reads leaves one of them describing a set
        // the others no longer have — the hazard
        // `2026-08-21-artifact-layout-selection.md` §9's first constraint names.
        //
        // **A partition that fails to compose is an absence, not an error.** The only failure is
        // an unreadable postings file, and the answer to that is the masked-count route, which
        // reads no postings and is what every request took before this structure existed. Logged
        // rather than returned, because the caller's alternative would be to fail a request over a
        // derivation that has a correct fallback.
        let partition = source
            .filter(|source| source.signature_shaped())
            .and_then(|source| self.partition_for(prefix, layer, level, store, source));
        // **A spatial level's membership is assembled from its segments' resolutions**
        // (`crate::shapes`), in this generation's whole row space, once: open and the fold stage
        // every segment's piece before this runs, so what happens here is an O(containers) union
        // per segment; a segment nothing staged is resolved here, which is this build paying for
        // it and not a request-path fallback. The result is a per-row source and takes the same
        // road an enumerated level's takes from here — the tile index, the column where the layout
        // is row-major, the histogram — and from here on the form is maintained as an enumerated
        // level's is.
        //
        // **The fold-written column is claimed only while the generation has no extents.** That
        // column is over the base rows; a flushed segment's rows lie above them, and a column
        // that does not label them would count every point ingested since the fold as in no
        // shape — the staleness a spatial membership must not have. With extents the column is
        // composed over the assembled form instead.
        if let Some(PredicateSource::Spatial(spatial)) = predicate {
            let (joined, assembly) = spatial.level.assemble(spatial.segments);
            // **A spatial level is filtered by view exactly as an enumerated one is**
            // (`ArtifactStore::level_in_view`, `views.md` §3.5): a shape belongs to one view of
            // its group, and one resolved into every view's row space would draw a polygon
            // published into one quarter on every quarter's map, with a real masked count.
            let built = ArtifactRows::build_resolved(
                store.level_in_view(layer, level, view_key(view)),
                joined,
                spatial.total_rows,
                space,
            )
            .with_partition(partition);
            let column = if !layout.is_row_major() {
                None
            } else if space.extent_count() == 0 {
                self.column_for(
                    prefix,
                    view,
                    layer,
                    level,
                    key.level_version,
                    layout,
                    &built,
                )
            } else {
                let composed = RowColumn::compose_over_base(
                    built.membership(),
                    built.base_rows,
                    built.index().row_count(),
                    layout,
                    self.scratch(),
                )
                .map(Arc::new);
                if composed.is_some() {
                    self.columns_composed
                        .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
                }
                composed
            };
            let from_column = column.is_some();
            let rows = Arc::new(built.with_column(column));
            if layout.is_row_major() && !from_column {
                self.fallbacks
                    .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
                tracing::warn!(
                    layer = %layer,
                    level,
                    view = %view,
                    recorded = ?layout,
                    "this spatial level is recorded row-major and its resolved memberships do \
                     not partition, so it is served artifact-major. Every answer is unchanged; \
                     the layout is not"
                );
            }
            tracing::info!(
                layer = %layer,
                level,
                view = %view,
                ordinals = rows.index().len(),
                segments = spatial.segments.len(),
                staged = assembly.staged,
                resolved = assembly.resolved,
                rows_tested = assembly.rows_tested,
                resolve_ms = assembly.resolve_ms,
                elapsed_ms = assembly.elapsed_ms,
                layout = ?rows.layout(),
                "a spatial level's row form is assembled from its segments' resolutions"
            );
            self.builds
                .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
            let version = key.level_version;
            self.insert_newest(
                map_key,
                Held {
                    key,
                    at: segments_version,
                    rows: Arc::clone(&rows),
                },
            );
            return (rows, version);
        }
        let mut adopted = self.claim_index(prefix, view, layer, level, key.level_version);
        let from_prefix = adopted.is_some();
        // **A level recorded row-major whose column this prefix holds is transposed, not projected
        // twice** (§5.1). The column *is* the level's membership addressed by row, so the
        // artifact-major half every other answer is computed from can be read off it in one
        // sequential pass instead of decoding and permuting every membership again — 24 s at rung
        // 3's `mesh/descriptors` before this, and the two forms are equal artifact for artifact.
        //
        // **Claimed here rather than in `column_for`**, because the form is built from it: an
        // attribute predicate's column is not a stored membership and is not a candidate for this,
        // and a column that turns out not to cover the level leaves `built` on the projecting
        // route with nothing lost but the walk of the records.
        let claimed = match predicate {
            Some(PredicateSource::Attribute(_)) => None,
            _ if !layout.is_row_major() => None,
            _ => self
                .claim_column(prefix, view, layer, level, key.level_version)
                .filter(|claimed| claimed.layout() == layout)
                .map(Arc::new),
        };
        let transposed = claimed.as_ref().and_then(|column| {
            ArtifactRows::build_from_column(
                store.level_in_view(layer, level, view_key(view)),
                space,
                column,
                &mut adopted,
                column_only,
            )
        });
        let from_prefix_column = transposed.is_some();
        // **True where the column's bytes were turned back into per-artifact bitmaps**, which is
        // not every level built from one: a level whose extents the prefix also holds is built
        // from the column without transposing anything (`rows_held` beside it says which).
        let from_transpose = transposed
            .as_ref()
            .is_some_and(|rows| rows.membership().rows_held());
        if claimed.is_some() && !from_prefix_column {
            tracing::warn!(
                layer = %layer,
                level,
                view = %view,
                "an adopted row-major column does not cover this level's row space or its \
                 ordinals, so the level's row form is projected; every answer is unchanged"
            );
        }
        let built = transposed
            .unwrap_or_else(|| {
                ArtifactRows::build_over(
                    store.level_in_view(layer, level, view_key(view)),
                    space,
                    adopted.take(),
                )
            })
            .with_partition(partition);
        // **The column, claimed from the prefix or composed from the form just built** — and the
        // one place the recorded layout and the served one may differ. A level recorded row-major
        // whose memberships turn out to overlap has no label column to compose, and the fallback is
        // the artifact-major route, which is correct and merely slower than the record asked for.
        let column = match predicate {
            // **The membership *is* the column** (§5.1): the labels come from the value column the
            // predicate names rather than from any stored membership, and the level's own records
            // supply only the ordinal each value's artifact sits at.
            Some(PredicateSource::Attribute(attribute)) => self.attribute_column(
                prefix,
                view,
                layer,
                level,
                key.level_version,
                store,
                space,
                attribute,
            ),
            // The claim above already took it, where the prefix held one: `column_for` would
            // find nothing there and recompose what is in hand.
            // **A fold-written column is served only while row space has no extents.** It is
            // addressed by row over the rows the fold folded; a flushed segment's rows lie above
            // them, and a column that does not label them would count every point ingested since
            // the fold as in no artifact — the staleness the form's own extension exists against.
            // With extents the column is composed over the form the transpose just produced, which
            // is the same choice the spatial branch above makes and for the same reason.
            _ => match claimed {
                Some(claimed) if space.extent_count() == 0 => {
                    self.columns_adopted
                        .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
                    Some(claimed)
                }
                _ => self.column_for(
                    prefix,
                    view,
                    layer,
                    level,
                    key.level_version,
                    layout,
                    &built,
                ),
            },
        };
        let from_column = column.is_some();
        let mut built = built.with_column(column);
        // **A column-only form without its column is not a form at all.** The branch above takes
        // the claimed column whenever the transpose was skipped — the two are decided by the same
        // pair of terms — so this cannot fire; it is here because the alternative to firing is a
        // level whose every membership reads as absent, and the transpose is the answer that costs
        // rather than the answer that is wrong.
        if !from_column && !built.membership().rows_held() {
            tracing::error!(
                layer = %layer,
                level,
                view = %view,
                "ALARM: a level built from its column alone has no column to serve from; its rows \
                 are transposed back"
            );
            built.membership =
                MembershipRows::build(store.level_in_view(layer, level, view_key(view)), space);
            built.index = TileIndex::build(&built.membership, total_rows(space));
        }
        // The borrowed memberships come last, after the form is otherwise complete. They replace
        // the empty membership each record declared, and they invalidate the index derived over it.
        // See [`ArtifactRows::inherit`].
        let inherited = built.inherit(store, space, layer, level, view_key(view));
        if inherited > 0 {
            tracing::info!(
                layer = %layer,
                level,
                view = %view,
                artifacts = inherited,
                borrowed = ?built.inherited,
                "artifacts of this level declare no membership of their own and take the \
                 membership of what they attach to; the level is served artifact-major"
            );
        }
        let rows = Arc::new(built);
        // **Not the fallback below**, where a recorded layout could not be honoured: a level that
        // borrows is served artifact-major because the borrowed rows are in no column, which the
        // line above has already said.
        if layout.is_row_major() && !from_column && inherited == 0 {
            self.fallbacks
                .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
            tracing::warn!(
                layer = %layer,
                level,
                view = %view,
                recorded = ?layout,
                "this level is recorded row-major and has no column to scan, so it is served \
                 artifact-major: its memberships do not partition, or the fold's file would not \
                 open. Every answer is unchanged; the layout is not"
            );
        }
        // **The `everywhere` set, reported where the form is built.** It is the number that says a
        // layer is *scattered* — every artifact of one lands here at every size measured (§5) — and
        // so the number that predicts a whole-map request paying the full masked probe for the
        // population rather than for the viewport's perimeter. Per generation move, not per
        // request; it names no artifact and no principal.
        //
        // **`blocks_per_artifact` beside it is decision 0092's (c)** — the figure the automatic
        // layout pick reads, so an operator can see what the choice was made from. It is the mean
        // over the form just built, which is the same walk the report at publication makes.
        tracing::info!(
            layer = %layer,
            level,
            view = %view,
            ordinals = rows.index().len(),
            everywhere = rows.index().everywhere(),
            adopted = from_prefix,
            transposed = from_transpose,
            rows_held = rows.membership().rows_held(),
            layout = ?rows.layout(),
            // **Absent on a column-only form rather than reported as zero**: the figure is Roaring
            // containers per artifact over the row form, and a form that holds no bitmaps has none
            // to count. A zero there reads as *perfect locality*, which is the opposite of what it
            // would mean.
            blocks_per_artifact = rows
                .membership()
                .rows_held()
                .then(|| rows.membership().blocks_per_artifact()),
            "a level's row form and tile index are built"
        );
        self.builds
            .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        let version = key.level_version;
        self.insert_newest(
            map_key,
            Held {
                key,
                at: segments_version,
                rows: Arc::clone(&rows),
            },
        );
        (rows, version)
    }

    /// Take the fold-written index for this `(view, layer, level)` if one was adopted and its
    /// coordinate is still the one being built at.
    ///
    /// **Removed rather than borrowed.** An index belongs to one view's row form; once that form
    /// has it there is no second reader, and leaving the entry behind would hold a second copy of
    /// the level's extents for the process's life. A caller that finds nothing derives, which is
    /// the same answer at the cost the fold was trying to save.
    ///
    /// **An entry held for another prefix is left where it is.** The fold adopts under the prefix
    /// it published while requests that loaded the outgoing generation are still building under
    /// theirs, against a store whose version is already the new one; a claim from one of those
    /// that removed the entry would send the warm down the whole projection the entry exists to
    /// replace. Only a same-prefix version mismatch drops an entry, and [`Self::adopt_indexes`]
    /// purges whatever was held for a prefix other than the one it adopts under, so an entry
    /// still cannot outlive its prefix.
    fn claim_index(
        &self,
        prefix: &str,
        view: &str,
        layer: &str,
        level: u32,
        level_version: u64,
    ) -> Option<TileIndex> {
        let map_key = (view.to_string(), layer.to_string(), level);
        let mut held = self.indexes_held.lock().unwrap_or_else(|e| e.into_inner());
        let (key, _) = held.get(&map_key)?;
        if key.prefix != prefix {
            return None;
        }
        if key.level_version != level_version {
            // The coordinate has moved under the entry, so nothing will ever claim it. Dropped
            // here rather than left: what makes it stale is what makes it dead weight.
            held.remove(&map_key);
            return None;
        }
        let (_, index) = held.remove(&map_key)?;
        self.indexes_adopted
            .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        Some(index)
    }

    /// **An attribute predicate's row column: the value column, permuted through this view's row
    /// space** (`design/artifact-serving-at-scale.md` §5.1).
    ///
    /// The base and the tail are built and cached separately, because they move on different
    /// cadences and only one of them is expensive:
    ///
    /// - the **base** covers `[0, base_rows)` and is a function of the prefix and the level's
    ///   version, so it survives every flush and is rebuilt only by a fold or a mint. It is four
    ///   bytes a row — 4 GB at 10⁹ — and rebuilding it per flush is exactly the cost
    ///   `RowSpace::project_base` exists to avoid;
    /// - the **tail** covers the rows a flush appended and is rebuilt whenever the geometry moves,
    ///   which is what makes a point ingested with value *v* count on the next request. It is
    ///   bounded by the flushed tail, which the merge ladder bounds and the fold resets.
    ///
    /// **`None` where the column is not held at all**, which is the fail-closed answer: no artifact
    /// of the layer is then a candidate anywhere, rather than every artifact being one.
    ///
    /// **An entity with no value is in no artifact.** A row whose entity carries nothing, and one
    /// whose value names no artifact of this level — a code minted after this level's records were
    /// written, or one whose artifact a fold has retired — is a hole, which contributes to nobody's
    /// count. That is the same answer a member row with a null key gets on an enumerated layer.
    #[allow(clippy::too_many_arguments)]
    fn attribute_column(
        &self,
        prefix: &str,
        view: &str,
        layer: &str,
        level: u32,
        level_version: u64,
        store: &ArtifactStore,
        space: &RowSpace,
        source: &AttributeSource<'_>,
    ) -> Option<Arc<RowColumn>> {
        // `code → ordinal`, from the level's own records: the key an artifact carries is the value
        // it stands for, and `code_of_key` is the inverse of the rule the mint used to write it.
        // A key that does not resolve is skipped rather than guessed at — its rows then belong to
        // nobody, which understates and never over-states.
        // **Unfiltered by view, and it cannot reach a group-scoped layer**: this is a
        // predicate layer's column, and a predicate layer's artifacts are derived from a value
        // column rather than published — `LayerRegistry::prepare_derive` names no view, so a
        // group-scoped layer of this kind refuses every artifact and holds none (`ingest.md`
        // §1.5).
        let mut ordinal_of_code: std::collections::BTreeMap<u32, u32> =
            std::collections::BTreeMap::new();
        let mut ordinals = 0u32;
        for (ordinal, record) in store.level(layer, level) {
            ordinals = ordinals.max(ordinal + 1);
            if let Some(code) = record.key.as_deref().and_then(source.code_of_key) {
                ordinal_of_code.insert(code, ordinal);
            }
        }

        let base_rows = space.base_rows();
        let map_key = (view.to_string(), layer.to_string(), level);
        let base_key = IndexKey {
            prefix: prefix.to_string(),
            level_version,
        };
        let held = {
            let bases = self
                .predicate_bases
                .lock()
                .unwrap_or_else(|e| e.into_inner());
            bases
                .get(&map_key)
                .filter(|(key, _)| *key == base_key)
                .map(|(_, column)| Arc::clone(column))
        };
        let base = match held {
            Some(base) => base,
            None => {
                let mut labels =
                    vec![tessera_store::membership::ROW_COLUMN_HOLE; base_rows as usize];
                if let Some(values) = source.values.base() {
                    for entity in values.present().iter() {
                        let Some(row) =
                            space.row_of(tessera_types::EntityId::new(u64::from(entity)))
                        else {
                            continue;
                        };
                        if row.raw() >= base_rows {
                            continue;
                        }
                        if let Some(ordinal) = values
                            .value_of(entity)
                            .and_then(|code| ordinal_of_code.get(&code.raw()))
                        {
                            labels[row.raw() as usize] = *ordinal;
                        }
                    }
                }
                let base = Arc::new(RowColumn::from_labels(ordinals, &labels));
                self.columns_composed
                    .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
                self.predicate_bases
                    .lock()
                    .unwrap_or_else(|e| e.into_inner())
                    .insert(map_key, (base_key, Arc::clone(&base)));
                base
            }
        };

        // The live half: the rows a flush has appended since the base was written. Empty where
        // nothing has flushed, in which case the base is served as it stands.
        let tail_rows = space.total_rows().saturating_sub(u64::from(base_rows));
        if tail_rows == 0 {
            return Some(base);
        }
        let mut tail = vec![tessera_store::membership::ROW_COLUMN_HOLE; tail_rows as usize];
        for values in source.values.extents() {
            for entity in values.present().iter() {
                let Some(row) = space.row_of(tessera_types::EntityId::new(u64::from(entity)))
                else {
                    continue;
                };
                let Some(at) = row.raw().checked_sub(base_rows) else {
                    continue;
                };
                if at as usize >= tail.len() {
                    continue;
                }
                if let Some(ordinal) = values
                    .value_of(entity)
                    .and_then(|code| ordinal_of_code.get(&code.raw()))
                {
                    tail[at as usize] = *ordinal;
                }
            }
        }
        Some(Arc::new(base.with_tail(
            crate::row_column::TailLabels::new(base_rows, tail),
        )))
    }

    /// This level's row-major column, claimed from the prefix where the fold wrote one at this
    /// coordinate and composed from `rows` where it did not.
    ///
    /// `None` where the level is artifact-major — which has no column — and where a label column
    /// declined to compose because the memberships do not partition. Both are absences rather than
    /// errors: the artifact-major route answers every question the column would have.
    #[allow(clippy::too_many_arguments)]
    fn column_for(
        &self,
        prefix: &str,
        view: &str,
        layer: &str,
        level: u32,
        level_version: u64,
        layout: ServingLayout,
        rows: &ArtifactRows,
    ) -> Option<Arc<RowColumn>> {
        if !layout.is_row_major() {
            return None;
        }
        if let Some(claimed) = self.claim_column(prefix, view, layer, level, level_version) {
            // **The adopted form has to be the recorded one.** A file adopted under one tag and
            // recorded under another would serve a list where a label column belongs — the
            // manifest's own claim, which `RowColumn::open` already checked against the magic. This
            // is the second half of it, against the record the fold wrote beside the file.
            if claimed.layout() == layout {
                self.columns_adopted
                    .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
                return Some(Arc::new(claimed));
            }
        }
        // **Over the base rows, with the extent rows as the amendment** — the split a merge's
        // rebase rests on (`RowColumn::compose_over_base`). A pack composed over a row space that
        // already carried extents would hold labels at rows the next merge renumbers.
        let composed = RowColumn::compose_over_base(
            rows.membership(),
            rows.base_rows,
            rows.index().row_count(),
            layout,
            self.scratch(),
        )
        .map(Arc::new);
        if composed.is_some() {
            self.columns_composed
                .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        }
        composed
    }

    /// Take the fold-written column for this `(view, layer, level)` if one was adopted and its
    /// coordinate is still the one being built at.
    ///
    /// **Removed rather than borrowed**, for [`Self::claim_index`]'s reason: a column belongs to one
    /// view's row form, and leaving the entry behind would hold a second copy of four bytes a row
    /// for the process's life. An entry held for another prefix is left, for that method's other
    /// reason, and [`Self::adopt_columns`] purges what another prefix held.
    fn claim_column(
        &self,
        prefix: &str,
        view: &str,
        layer: &str,
        level: u32,
        level_version: u64,
    ) -> Option<RowColumn> {
        let map_key = (view.to_string(), layer.to_string(), level);
        let mut held = self.columns_held.lock().unwrap_or_else(|e| e.into_inner());
        let (key, _) = held.get(&map_key)?;
        if key.prefix != prefix {
            return None;
        }
        if key.level_version != level_version {
            held.remove(&map_key);
            return None;
        }
        held.remove(&map_key).map(|(_, column)| column)
    }

    /// This level's containment partition for the generation `store` is in, composing it if what
    /// is held is stale.
    ///
    /// **A partition that fails to compose is an absence, not an error.** The only failure is an
    /// unreadable postings file, and the answer to that is the masked-count route, which reads no
    /// postings and is what every request took before this structure existed. Logged rather than
    /// returned, because the caller's alternative would be to fail a request over a derivation
    /// that has a correct fallback.
    fn partition_for(
        &self,
        prefix: &str,
        layer: &str,
        level: u32,
        store: &ArtifactStore,
        source: &PartitionSource<'_>,
    ) -> Option<ContainmentPartition> {
        let key = PartitionKey {
            prefix: prefix.to_string(),
            level_version: store.level_version(layer, level),
        };
        let map_key = (layer.to_string(), level);
        if let Some((held, partition)) = self
            .partitions_held
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .get(&map_key)
        {
            if *held == key {
                return Some(partition.clone());
            }
        }
        let partition = match ContainmentPartition::compose(store, layer, level, source.postings) {
            Ok(partition) => partition,
            Err(error) => {
                tracing::warn!(
                    layer = %layer,
                    level,
                    %error,
                    "the containment partition could not be composed from the postings; \
                     containment stays on the masked-count route for this level"
                );
                return None;
            }
        };
        self.partitions
            .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        self.partitions_held
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .insert(map_key, (key, partition.clone()));
        Some(partition)
    }
}
