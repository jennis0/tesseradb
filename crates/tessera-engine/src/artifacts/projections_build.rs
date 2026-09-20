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
    /// The version is read from the store this builds from, never handed in, so there is no
    /// window for a separately-read version to label a form built from records that have since
    /// moved. The returned version is the form's own, which is not always the store's: the level
    /// version is a floor ([`ProjectionKey::stale_form_of`]).
    ///
    /// The build runs outside this cache's lock, so a slow projection does not block every other
    /// layer's requests behind it; two threads racing the same key both build from the same level
    /// version over the same row space, so the waste is one projection, not a wrong answer.
    //
    // Every argument is a thing a level's derived form is *of*: where it came from, what it is
    // built from, which form it is served in, and what the containment partition needs beside
    // them.
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
        let at = Coordinate {
            prefix,
            view,
            layer,
            level,
            level_version: store.level_version(layer, level),
        };
        // See [`ProjectionKey::live`].
        let key = at.projection_key(match predicate {
            Some(PredicateSource::Attribute(_)) => segments_version,
            _ => 0,
        });
        if let Some(served) = self.cached_form(&at, &key, store, space) {
            return served;
        }

        // Both derivations, and the partition, from one borrow of the store at one level version:
        // the row form, the records and the containment partition must describe the same
        // population, and a growth landing between two reads would leave one of them describing a
        // set the others no longer have.
        //
        // A partition that fails to compose is an absence, not an error: the only failure is an
        // unreadable postings file, and the answer to that is the masked-count route, which reads
        // no postings. Logged rather than returned, since the caller's alternative would be to
        // fail a request over a derivation that has a correct fallback.
        let partition = source
            .filter(|source| source.signature_shaped())
            .and_then(|source| self.partition_for(&at, store, source));
        let built = match predicate {
            Some(PredicateSource::Spatial(spatial)) => {
                self.assembled(&at, spatial, store, space, layout)
            }
            Some(PredicateSource::Attribute(attribute)) => {
                self.claimed_or_projected(&at, store, space, layout, Some(attribute), column_only)
            }
            None => self.claimed_or_projected(&at, store, space, layout, None, column_only),
        };
        self.filed(&at, key, segments_version, built.with_partition(partition))
    }

    /// The form held for this coordinate where it still answers for the caller's row space, and the
    /// version it is the level at.
    fn cached_form(
        &self,
        at: &Coordinate<'_>,
        key: &ProjectionKey,
        store: &ArtifactStore,
        space: &RowSpace,
    ) -> Option<(Arc<ArtifactRows>, u64)> {
        let cached = self.cached.lock().unwrap_or_else(|e| e.into_inner());
        let held = cached.get(&at.address())?;
        // The key and the row space both: the key says the form describes this level's records,
        // and [`ArtifactRows::covers`] says its rows are rows of this row space, which a flush and
        // a merge move without bumping the key. The level version is a floor, not an equality, so
        // the held form is served up to a tick stale, which can only undercount, never overcount.
        if held.key.stale_form_of(key)
            && held.rows.covers(space)
            && held.rows.inherited_current(store)
        {
            // A form still waiting for a tick's delta is the level at the earlier version, which
            // is what anything derived from it must be filed under.
            return Some((Arc::clone(&held.rows), held.key.level_version));
        }
        None
    }

    /// The build's tail: the build counted, the form filed under this coordinate, and handed back
    /// with `key`'s version rather than the store's, for [`Self::get_or_build`]'s reason.
    fn filed(
        &self,
        at: &Coordinate<'_>,
        key: ProjectionKey,
        segments_version: u64,
        built: ArtifactRows,
    ) -> (Arc<ArtifactRows>, u64) {
        self.builds
            .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        let version = key.level_version;
        let rows = Arc::new(built);
        self.insert_newest(
            at.address(),
            Held {
                key,
                at: segments_version,
                rows: Arc::clone(&rows),
            },
        );
        (rows, version)
    }

    /// A spatial level's membership is assembled from its segments' resolutions, once, over this
    /// generation's whole row space. From here the form is maintained as an enumerated level's is.
    ///
    /// The fold-written column is claimed only while the generation has no extents: a column that
    /// does not label a flushed segment's rows would count every point ingested since the fold as
    /// in no shape. With extents the column is composed over the assembled form instead.
    fn assembled(
        &self,
        at: &Coordinate<'_>,
        spatial: &SpatialSource<'_>,
        store: &ArtifactStore,
        space: &RowSpace,
        layout: ServingLayout,
    ) -> ArtifactRows {
        let (joined, assembly) = spatial.level.assemble(spatial.segments);
        // Filtered by view: a shape belongs to one view of its group, and resolving it into every
        // view's row space would draw it on every view's map with a real masked count.
        let built = ArtifactRows::build_resolved(
            store.level_in_view(at.layer, at.level, view_key(at.view)),
            joined,
            spatial.total_rows,
            space,
        );
        let column = if !layout.is_row_major() {
            None
        } else if space.extent_count() == 0 {
            self.column_for(at, layout, &built)
        } else {
            self.composed_column(&built, layout)
        };
        let from_column = column.is_some();
        let rows = built.with_column(column);
        if layout.is_row_major() && !from_column {
            self.fallbacks
                .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
            tracing::warn!(
                layer = %at.layer,
                level = at.level,
                view = %at.view,
                recorded = ?layout,
                "this spatial level is recorded row-major and its resolved memberships do \
                 not partition, so it is served artifact-major. Every answer is unchanged; \
                 the layout is not"
            );
        }
        tracing::info!(
            layer = %at.layer,
            level = at.level,
            view = %at.view,
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
        rows
    }

    /// This level's row form, transposed out of the column the prefix holds or projected from the
    /// level's records — every level but a spatial one, whose rows come from its shapes
    /// ([`Self::assembled`]).
    ///
    /// `attribute` is the value column where this level's membership is one, which changes what is
    /// claimed and where the served column comes from and nothing else.
    fn claimed_or_projected(
        &self,
        at: &Coordinate<'_>,
        store: &ArtifactStore,
        space: &RowSpace,
        layout: ServingLayout,
        attribute: Option<&AttributeSource<'_>>,
        column_only: bool,
    ) -> ArtifactRows {
        let mut adopted = self.claim_index(at);
        let from_prefix = adopted.is_some();
        // A row-major column this prefix holds is transposed, not projected twice: an attribute
        // predicate's column is not a stored membership and is not a candidate for this.
        let claimed = match attribute {
            Some(_) => None,
            _ if !layout.is_row_major() => None,
            _ => self
                .claim_column(at)
                .filter(|claimed| claimed.layout() == layout)
                .map(Arc::new),
        };
        let transposed = claimed.as_ref().and_then(|column| {
            ArtifactRows::build_from_column(
                store.level_in_view(at.layer, at.level, view_key(at.view)),
                space,
                column,
                &mut adopted,
                column_only,
            )
        });
        let from_prefix_column = transposed.is_some();
        // True where the column's bytes were turned back into bitmaps, not merely read off.
        let from_transpose = transposed
            .as_ref()
            .is_some_and(|rows| rows.membership().rows_held());
        if claimed.is_some() && !from_prefix_column {
            tracing::warn!(
                layer = %at.layer,
                level = at.level,
                view = %at.view,
                "an adopted row-major column does not cover this level's row space or its \
                 ordinals, so the level's row form is projected; every answer is unchanged"
            );
        }
        let built = transposed.unwrap_or_else(|| {
            ArtifactRows::build_over(
                store.level_in_view(at.layer, at.level, view_key(at.view)),
                space,
                adopted.take(),
            )
        });
        // The column, claimed from the prefix or composed from the form just built. A level
        // recorded row-major whose memberships turn out to overlap has no label column to compose,
        // and falls back to the artifact-major route, correct and merely slower than asked for.
        let column = match attribute {
            // The membership *is* the column: the level's own records supply only the ordinal
            // each value's artifact sits at.
            Some(attribute) => self.attribute_column(at, store, space, attribute),
            None => self.claimed_or_composed(at, claimed, layout, space, &built),
        };
        let from_column = column.is_some();
        let mut built = built.with_column(column);
        self.transpose_back(at, &mut built, store, space);
        let inherited = self.inherited(at, &mut built, store, space);
        if layout.is_row_major() && !from_column && inherited == 0 {
            self.fallbacks
                .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
            tracing::warn!(
                layer = %at.layer,
                level = at.level,
                view = %at.view,
                recorded = ?layout,
                "this level is recorded row-major and has no column to scan, so it is served \
                 artifact-major: its memberships do not partition, or the fold's file would not \
                 open. Every answer is unchanged; the layout is not"
            );
        }
        // `everywhere` says a layer is scattered: a whole-map request then pays the full masked
        // probe for the population rather than the viewport's perimeter. `blocks_per_artifact` is
        // the figure the automatic layout pick reads.
        tracing::info!(
            layer = %at.layer,
            level = at.level,
            view = %at.view,
            ordinals = built.index().len(),
            everywhere = built.index().everywhere(),
            adopted = from_prefix,
            transposed = from_transpose,
            rows_held = built.membership().rows_held(),
            layout = ?built.layout(),
            // Absent, not zero, on a column-only form: a zero would read as perfect locality.
            blocks_per_artifact = built
                .membership()
                .rows_held()
                .then(|| built.membership().blocks_per_artifact()),
            "a level's row form and tile index are built"
        );
        built
    }

    /// The served column of a level whose membership is stored: the one the prefix held, and
    /// otherwise the column composed from the form just built.
    ///
    /// A fold-written column is served only while the row space has no extents: a column that does
    /// not label a flushed segment's rows would count every point ingested since the fold as in no
    /// artifact. With extents the column is composed over the form the transpose just produced.
    fn claimed_or_composed(
        &self,
        at: &Coordinate<'_>,
        claimed: Option<Arc<RowColumn>>,
        layout: ServingLayout,
        space: &RowSpace,
        built: &ArtifactRows,
    ) -> Option<Arc<RowColumn>> {
        match claimed {
            Some(claimed) if space.extent_count() == 0 => {
                self.columns_adopted
                    .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
                Some(claimed)
            }
            _ => self.column_for(at, layout, built),
        }
    }

    /// A column-only form without its column is not a form at all, so its rows are projected back
    /// into it. Does not fire in the ordinary course.
    fn transpose_back(
        &self,
        at: &Coordinate<'_>,
        built: &mut ArtifactRows,
        store: &ArtifactStore,
        space: &RowSpace,
    ) {
        if built.column().is_some() || built.membership().rows_held() {
            return;
        }
        tracing::error!(
            layer = %at.layer,
            level = at.level,
            view = %at.view,
            "ALARM: a level built from its column alone has no column to serve from; its rows \
             are transposed back"
        );
        built.membership = MembershipRows::build(
            store.level_in_view(at.layer, at.level, view_key(at.view)),
            space,
        );
        built.index = TileIndex::build(&built.membership, total_rows(space));
    }

    /// How many of this level's artifacts took a membership that is not their own. Applied last,
    /// after the form is otherwise complete, replacing each record's declared empty membership.
    fn inherited(
        &self,
        at: &Coordinate<'_>,
        built: &mut ArtifactRows,
        store: &ArtifactStore,
        space: &RowSpace,
    ) -> usize {
        let inherited = built.inherit(store, space, at.layer, at.level, view_key(at.view));
        if inherited > 0 {
            tracing::info!(
                layer = %at.layer,
                level = at.level,
                view = %at.view,
                artifacts = inherited,
                borrowed = ?built.inherited,
                "artifacts of this level declare no membership of their own and take the \
                 membership of what they attach to; the level is served artifact-major"
            );
        }
        inherited
    }

    /// Take the fold-written index for this `(view, layer, level)` if one was adopted and its
    /// coordinate is still the one being built at. Removed rather than borrowed, so leaving no
    /// second copy behind. An entry held for another prefix is left where it is, purged instead by
    /// [`Self::adopt_indexes`].
    fn claim_index(&self, at: &Coordinate<'_>) -> Option<TileIndex> {
        let map_key = at.address();
        let mut held = self.indexes_held.lock().unwrap_or_else(|e| e.into_inner());
        let (key, _) = held.get(&map_key)?;
        if key.prefix != at.prefix {
            return None;
        }
        if key.level_version != at.level_version {
            // The coordinate has moved, so nothing will ever claim it: dropped here instead.
            held.remove(&map_key);
            return None;
        }
        let (_, index) = held.remove(&map_key)?;
        self.indexes_adopted
            .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        Some(index)
    }

    /// An attribute predicate's row column: the value column, permuted through this view's row
    /// space. The base and the tail are built and cached separately, since they move on different
    /// cadences: the base covers `[0, base_rows)` and is rebuilt only by a fold or a mint; the
    /// tail covers the rows a flush appended and is rebuilt whenever the geometry moves.
    ///
    /// `None` where the column is not held at all is the fail-closed answer: no artifact of the
    /// layer is then a candidate anywhere, rather than every artifact being one. An entity with no
    /// value, or whose value names no artifact of this level, contributes to nobody's count.
    fn attribute_column(
        &self,
        at: &Coordinate<'_>,
        store: &ArtifactStore,
        space: &RowSpace,
        source: &AttributeSource<'_>,
    ) -> Option<Arc<RowColumn>> {
        // `code → ordinal`, from the level's own records. A key that does not resolve is skipped
        // rather than guessed at, which understates and never overstates.
        //
        // Unfiltered by view, and it cannot reach a group-scoped layer: a predicate layer's
        // artifacts are derived from a value column rather than published.
        let mut ordinal_of_code: std::collections::BTreeMap<u32, u32> =
            std::collections::BTreeMap::new();
        let mut ordinals = 0u32;
        for (ordinal, record) in store.level(at.layer, at.level) {
            ordinals = ordinals.max(ordinal + 1);
            if let Some(code) = record.key.as_deref().and_then(source.code_of_key) {
                ordinal_of_code.insert(code, ordinal);
            }
        }

        let base_rows = space.base_rows();
        let map_key = at.address();
        let base_key = at.derived_key();
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

        // The live half: the rows a flush has appended since the base was written.
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

    /// This level's row-major column, claimed from the prefix or composed from `rows`. `None`
    /// where the level is artifact-major, or a label column declined to compose because the
    /// memberships do not partition: both are absences rather than errors.
    fn column_for(
        &self,
        at: &Coordinate<'_>,
        layout: ServingLayout,
        rows: &ArtifactRows,
    ) -> Option<Arc<RowColumn>> {
        if !layout.is_row_major() {
            return None;
        }
        if let Some(claimed) = self.claim_column(at) {
            // The adopted form has to be the recorded one.
            if claimed.layout() == layout {
                self.columns_adopted
                    .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
                return Some(Arc::new(claimed));
            }
        }
        self.composed_column(rows, layout)
    }

    /// This level's row-major column, composed from the form just built — `None` where the
    /// memberships do not partition. Over the base rows, with the extent rows as the amendment: a
    /// pack composed over a row space that already carried extents would hold labels at rows the
    /// next merge renumbers.
    fn composed_column(
        &self,
        rows: &ArtifactRows,
        layout: ServingLayout,
    ) -> Option<Arc<RowColumn>> {
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

    /// Take the fold-written column for this `(view, layer, level)`. [`Self::claim_index`]'s rule.
    fn claim_column(&self, at: &Coordinate<'_>) -> Option<RowColumn> {
        let map_key = at.address();
        let mut held = self.columns_held.lock().unwrap_or_else(|e| e.into_inner());
        let (key, _) = held.get(&map_key)?;
        if key.prefix != at.prefix {
            return None;
        }
        if key.level_version != at.level_version {
            held.remove(&map_key);
            return None;
        }
        held.remove(&map_key).map(|(_, column)| column)
    }

    /// This level's containment partition, composing it if what is held is stale.
    /// [`Self::get_or_build`]'s reason a failure is an absence, not an error.
    fn partition_for(
        &self,
        at: &Coordinate<'_>,
        store: &ArtifactStore,
        source: &PartitionSource<'_>,
    ) -> Option<ContainmentPartition> {
        let key = at.derived_key();
        let map_key = at.partition_address();
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
        let partition = match ContainmentPartition::compose(store, at.layer, at.level, source.postings)
        {
            Ok(partition) => partition,
            Err(error) => {
                tracing::warn!(
                    layer = %at.layer,
                    level = at.level,
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
