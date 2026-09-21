//! Building a level's row form, and the memberships an attached artifact borrows.

use std::sync::Arc;

use croaring::Bitmap;

use tessera_lifecycle::membership::{ArtifactRecord, ArtifactStore};
use tessera_types::layer::ServingLayout;

use tessera_store::permutation::RowSpace;

use crate::containment::ContainmentPartition;
use crate::row_column::RowColumn;
use crate::tile_index::TileIndex;

use super::*;

impl ArtifactRows {
    /// Build every half from one walk of one level, deriving the index over the projection.
    pub fn build<'a>(
        artifacts: impl Iterator<Item = (u32, &'a ArtifactRecord)>,
        space: &RowSpace,
    ) -> Self {
        Self::build_over(artifacts, space, None)
    }

    /// The same walk, offered a fold-written index to adopt instead of deriving one. Refused where
    /// the ordinal count disagrees: a shorter index would leave every ordinal past its end out of
    /// every walk, indistinguishable from artifacts that failed a criterion.
    pub fn build_over<'a>(
        artifacts: impl Iterator<Item = (u32, &'a ArtifactRecord)>,
        space: &RowSpace,
        adopted: Option<TileIndex>,
    ) -> Self {
        let mut records = ArtifactRecords::default();
        let mut membership = MembershipRows::default();
        for (ordinal, record) in artifacts {
            let idx = ordinal as usize;
            records.put(idx, record);
            membership.put(idx, record, space);
        }
        // An offered index is refused outright where the row space has extents: the fold wrote it
        // over the base rows, a flushed segment's rows lie above them, and an artifact whose extent
        // stops short of its own members would be settled inside a node it reaches outside of.
        let adopted = adopted.filter(|_| space.extent_count() == 0);
        let index = match adopted {
            Some(index) if index.len() == membership.len() => index,
            Some(index) => {
                tracing::warn!(
                    adopted_ordinals = index.len(),
                    level_ordinals = membership.len(),
                    "a fold-written tile index covers a different ordinal range from the level it \
                     was offered for; it is dropped and the level's index is derived"
                );
                TileIndex::build(&membership, total_rows(space))
            }
            None => TileIndex::build(&membership, total_rows(space)),
        };
        ArtifactRows {
            records: Arc::new(records),
            membership,
            index: Arc::new(index),
            partition: None,
            layout: ServingLayout::ArtifactMajor,
            column: None,
            base_rows: space.base_rows(),
            covered: covered_by(space),
            inherited: Vec::new(),
        }
    }

    /// The same family, with the row form transposed out of an adopted row column instead of
    /// projected from the level's memberships. A level recorded row-major arrives at open with the
    /// column the fold or the build wrote, and that column is the level's membership, addressed by
    /// row; transposing it costs one sequential read against a decode and permutation of the whole
    /// level, measured at roughly half the cost ([`RowColumn::transpose`]). The generating sets
    /// still project: they are a different set from the membership and are nowhere in the column.
    /// Nothing is transposed where the level can be served from the column alone: the form is then
    /// column-only, avoiding tens of gigabytes retained at scale per level ([`MembershipRows::rows_held`]).
    /// The extents still come off the column and never off a second file, since a fold-written
    /// extent column would be a separate artefact whose agreement with the column nothing checks.
    /// `column_only` is `false` for a level whose layer derives a hull, which needs the member
    /// positions themselves. `adopted` is taken only on success. `None` where the column cannot
    /// stand in for the projection — a tail attached, a base row count that is not this view's, or
    /// fewer ordinals than the level has records for — each would leave the form narrow.
    pub fn build_from_column<'a>(
        artifacts: impl Iterator<Item = (u32, &'a ArtifactRecord)>,
        space: &RowSpace,
        column: &RowColumn,
        adopted: &mut Option<TileIndex>,
        column_only: bool,
    ) -> Option<Self> {
        if column.base_rows() != space.base_rows() {
            return None;
        }
        let mut records = ArtifactRecords::default();
        let mut membership = MembershipRows::default();
        // The extent rows are projected here; the base rows come off the column.
        let mut above = Vec::new();
        for (ordinal, record) in artifacts {
            let idx = ordinal as usize;
            records.put(idx, record);
            membership.put_generating(idx, record, space);
            if space.extent_count() > 0 {
                above.push((idx, space.project_extents_from(&record.members, 0)));
            }
        }
        if column.len() < membership.len() {
            return None;
        }
        // The column answers candidacy, counts and declared sizes; a column-only level builds no
        // artifact-major half. Only with no extents: a column adopted over extent rows would not
        // label all of the row space, and a form built column-only goes on taking every later
        // flush through the column ([`ArtifactProjections::extend_flushed`]).
        if column_only && space.extent_count() == 0 {
            let live: Vec<bool> = membership.live_slots();
            let index = TileIndex::of_bytes(tessera_store::membership::pack_tile_index(
                total_rows(space),
                &column.extents(&live),
            ));
            membership.hold_no_rows();
            return Some(ArtifactRows {
                records: Arc::new(records),
                membership,
                index: Arc::new(index),
                partition: None,
                layout: ServingLayout::ArtifactMajor,
                column: None,
                base_rows: space.base_rows(),
                covered: covered_by(space),
                inherited: Vec::new(),
            });
        }
        // [`Self::build_over`]'s rule again: an index the fold wrote is over the base rows.
        let offered = adopted.take().filter(|_| space.extent_count() == 0);
        let offered = match offered {
            Some(index) if index.len() == membership.len() => Some(index),
            Some(index) => {
                tracing::warn!(
                    adopted_ordinals = index.len(),
                    level_ordinals = membership.len(),
                    "a fold-written tile index covers a different ordinal range from the level it \
                     was offered for; it is dropped and the level's index is derived"
                );
                None
            }
            None => None,
        };
        if !membership.absorb_transposed(column.transpose()?) {
            return None;
        }
        for (idx, rows) in above {
            membership.or_rows(idx, &rows);
        }
        let index = match offered {
            Some(index) => index,
            None => TileIndex::build(&membership, total_rows(space)),
        };
        Some(ArtifactRows {
            records: Arc::new(records),
            membership,
            index: Arc::new(index),
            partition: None,
            layout: ServingLayout::ArtifactMajor,
            column: None,
            base_rows: space.base_rows(),
            covered: covered_by(space),
            inherited: Vec::new(),
        })
    }

    /// The same family over a membership resolved elsewhere — a spatial level's, joined from the
    /// per-segment pieces the flush resolved. `rows` is the per-row source, parallel to the level's
    /// ordinals; `row_count` is the generation's total row count, base and every extent, because a
    /// shape's membership covers a flushed row the moment its segment publishes.
    pub fn build_resolved<'a>(
        artifacts: impl Iterator<Item = (u32, &'a ArtifactRecord)>,
        rows: Vec<Option<Bitmap>>,
        row_count: u32,
        space: &RowSpace,
    ) -> Self {
        let mut records = ArtifactRecords::default();
        let mut membership = MembershipRows::default();
        let mut rows = rows;
        for (ordinal, record) in artifacts {
            let idx = ordinal as usize;
            records.put(idx, record);
            let resolved = rows.get_mut(idx).and_then(Option::take).unwrap_or_default();
            membership.put_resolved(idx, record, resolved, space);
        }
        let index = TileIndex::build(&membership, row_count);
        ArtifactRows {
            records: Arc::new(records),
            membership,
            index: Arc::new(index),
            partition: None,
            layout: ServingLayout::ArtifactMajor,
            column: None,
            base_rows: space.base_rows(),
            covered: covered_by(space),
            inherited: Vec::new(),
        }
    }

    /// Serve this level row-major, from `column`. `None` puts the level back on the artifact-major
    /// route: a level whose memberships turned out to overlap, or whose fold-written file would
    /// not open, has no column to scan.
    pub fn with_column(mut self, column: Option<Arc<RowColumn>>) -> Self {
        self.layout = column
            .as_ref()
            .map(|column| column.layout())
            .unwrap_or(ServingLayout::ArtifactMajor);
        self.column = column;
        self
    }

    /// Attach the containment partition composed for the same level at the same level version.
    pub fn with_partition(mut self, partition: Option<ContainmentPartition>) -> Self {
        self.partition = partition;
        self
    }

    /// Row-space memberships and a serving column, with no `RowSpace` to project through — for
    /// tests about the resolver's two routes and not the projection.
    #[cfg(test)]
    pub(crate) fn synthetic(sets: &[Option<&[u32]>], column: Option<Arc<RowColumn>>) -> Self {
        let membership = MembershipRows {
            rows: sets
                .iter()
                .map(|s| s.map(|s| Arc::new(Bitmap::of(s))))
                .collect(),
            generating: vec![Vec::new(); sets.len()],
            rows_held: true,
        };
        let index = TileIndex::build(&membership, 0);
        ArtifactRows {
            records: Arc::new(ArtifactRecords {
                attachments: vec![None; sets.len()],
                parents: vec![Vec::new(); sets.len()],
                declared: vec![Vec::new(); sets.len()],
            }),
            membership,
            index: Arc::new(index),
            partition: None,
            layout: ServingLayout::ArtifactMajor,
            column: None,
            base_rows: 0,
            covered: Vec::new(),
            inherited: Vec::new(),
        }
        .with_column(column)
    }

    /// Place, count and gate an attached artifact that declares no members of its own over its
    /// target's membership. A label with no member rows is the label of its cluster: it sits in
    /// the tiles the cluster sits in, its masked count is the cluster's masked count, its existence
    /// criterion reads that number, and its proportional denominator is the cluster's declared
    /// size. A label that declares members keeps them, and `content_requires = "all"` gates on
    /// them unchanged. The rule is the store's, in
    /// [`tessera_lifecycle::membership::ArtifactStore::members_of`], applied here on the level's
    /// row form, once, so the tile index, the masked count, the criterion and every derived
    /// property follow from one membership.
    /// A borrowing artifact gains nothing its target's gate would withhold: every count is taken
    /// against this viewer's own composed mask, and existence is the target's too — an attached
    /// artifact is absent wherever its target is absent, on every route. A principal not served the
    /// cluster is not served its label, filtered or not. The membership is the target's as it
    /// stands now; the versions borrowed from are recorded in [`Self::inherited`], so a target that
    /// grows re-derives its labels at the next request that finds the form
    /// ([`Self::inherited_current`]). The form is served artifact-major once anything borrows: a
    /// build or a fold writes such a level's column over the borrowed membership as it stood then,
    /// a set the target's version moves without moving this level's, so the column is dropped
    /// rather than served from or amended. Returns how many ordinals took a membership that is not
    /// their own.
    pub(super) fn inherit(
        &mut self,
        store: &ArtifactStore,
        space: &RowSpace,
        layer: &str,
        level: u32,
        view: &str,
    ) -> usize {
        let mut taken = 0usize;
        let mut borrowed: Vec<(String, u32, u64)> = Vec::new();
        for (ordinal, record) in store.level_in_view(layer, level, view) {
            if !tessera_lifecycle::membership::borrows_membership(record) {
                continue;
            }
            // The store's own rule, not a second walk: the hops are what this form records so it
            // can tell when what it borrowed has moved.
            let mut hops = Vec::new();
            let members = store.members_of_tracked(record, &mut hops);
            if hops.is_empty() {
                // Nothing resolved: a hole, or an ordinal holding another entity.
                continue;
            }
            if taken == 0 && !self.membership.rows_held() {
                // A column-only form holds no bitmap to overwrite; rebuild artifact-major first.
                self.membership =
                    MembershipRows::build(store.level_in_view(layer, level, view), space);
            }
            self.membership
                .put_rows(ordinal as usize, space.project(members));
            taken += 1;
            for hop in hops {
                let version = store.level_version(&hop.0, hop.1);
                if !borrowed
                    .iter()
                    .any(|(l, lv, _)| l == &hop.0 && *lv == hop.1)
                {
                    borrowed.push((hop.0, hop.1, version));
                }
            }
        }
        if taken > 0 {
            self.layout = ServingLayout::ArtifactMajor;
            self.column = None;
            self.index = Arc::new(TileIndex::build(&self.membership, total_rows(space)));
        }
        self.inherited = borrowed;
        taken
    }

    /// Whether every membership this form borrowed is still the membership it borrowed.
    pub(super) fn inherited_current(&self, store: &ArtifactStore) -> bool {
        self.inherited
            .iter()
            .all(|(layer, level, version)| store.level_version(layer, *level) == *version)
    }
}
