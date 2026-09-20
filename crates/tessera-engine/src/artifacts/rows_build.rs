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
    /// Build every half from one walk of one level, at one level version — deriving the index over
    /// the projection this walk just produced.
    pub fn build<'a>(
        artifacts: impl Iterator<Item = (u32, &'a ArtifactRecord)>,
        space: &RowSpace,
    ) -> Self {
        Self::build_over(artifacts, space, None)
    }

    /// The same walk, offered a fold-written index to adopt instead of deriving one.
    ///
    /// **The offer is refused where the two do not describe the same population.** The adoption
    /// coordinate — prefix, view and level version — is what makes an offered index the index *of*
    /// this level, and the ordinal count is the one consequence of that a caller can check for
    /// nothing. A shorter column would leave every ordinal past its end out of every walk, so the
    /// artifacts simply stop being served, which is indistinguishable from artifacts that failed a
    /// criterion. Deriving instead costs one pass and is always right.
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
        // **An offered index is refused outright where the row space has extents**, and that is
        // the same rule as the ordinal check one line down rather than a new one: the fold wrote
        // it over the base rows, a flushed segment's rows lie above them, and an artifact whose
        // extent stops short of its own members is *settled* inside a node it reaches outside of —
        // the collapse the settled probe rests on, no longer true (`tile_index` module doc).
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
            records,
            membership,
            index,
            partition: None,
            layout: ServingLayout::ArtifactMajor,
            column: None,
            base_rows: space.base_rows(),
            covered: covered_by(space),
            inherited: Vec::new(),
        }
    }

    /// **The same family, with the row form transposed out of an adopted row column** instead of
    /// projected from the level's memberships (`artifact-serving-at-scale.md` §5.1).
    ///
    /// A level recorded row-major arrives at open with the column the fold or the build wrote, and
    /// that column *is* the level's membership — addressed by row. Projecting every membership a
    /// second time to reach the artifact-major half costs a decode and a permutation of the whole
    /// level — 23.6–25.1 s at rung 3's `mesh/descriptors`, 1.66×10⁹ entries — and transposing the
    /// column is the same set at one sequential read, **13.8–15.4 s** on the same host.
    /// [`RowColumn::transpose`] is the pass, and `row_column.rs`'s tests assert the two forms are equal artifact for artifact, holes and
    /// generating sets included.
    ///
    /// **The generating sets still project**: they are not in the column, they are a different set
    /// from the membership, and they are small — see [`MembershipRows::put_generating`].
    ///
    /// **Nothing is transposed where the level can be served from the column alone.** The form is
    /// then *column-only*: the column is the membership, each artifact's extent is folded out of
    /// the column's own bytes ([`RowColumn::extents`]), and the artifact-major bitmaps are not
    /// built at all ([`MembershipRows::rows_held`]). At the rung 6 corpus that is a measured
    /// ~28 GB retained and ~10 GB transient not paid at open, per level.
    ///
    /// **The extents come off the column and never off a second file**, which is what makes the
    /// form safe to serve from: a fold-written extent column is a separate artefact whose agreement
    /// with the column nothing checks, and a hole in it would make an artifact's rows read as
    /// absent where the membership has them — a silently short membership. `column_only` is the
    /// caller's decision and is `false` for a level whose layer derives a **hull**, which needs the
    /// member positions themselves rather than an accumulation over them.
    ///
    /// **`adopted` is taken only on success**, so a caller whose column turns out not to cover
    /// the level still has the fold-written index to hand to [`Self::build_over`].
    ///
    /// `None` where the column cannot stand in for the projection — a tail attached, a base row
    /// count that is not this view's, or fewer ordinals than the level has records for. Each would
    /// leave the form **narrow**, which is the direction a wrong row form must never be, so the
    /// caller projects instead and is no worse off than before this route existed.
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
        // **The extent rows are projected here and the base rows come off the column**, which is
        // the whole of what a fold-written column can and cannot supply: it is addressed by row
        // over the rows the fold folded, and a flush has appended rows since. Projecting the
        // extents costs the members inside one extent's entity range per artifact — a reset and a
        // walk of that range (`SegmentExtent::project`) — against the whole-level decode the
        // transpose is avoiding.
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
        // **The column answers candidacy, the masked counts and the declared sizes; its own bytes
        // answer the extents.** So a column-only level builds no artifact-major half: the
        // generating sets containment is tested against were projected above, being nowhere in the
        // column, and every other per-artifact question goes through [`Self::visible_rows`].
        //
        // **Only with no extents, and that is a condition on this build route alone**: the column
        // adopted here is addressed over the base rows and a flushed segment's rows lie above them,
        // so a form built while the row space already carries extents has a column that does not
        // label all of it. A form *built* with none goes on taking every later flush through the
        // column and the extents beside it ([`ArtifactProjections::extend_flushed`]); nothing gives
        // this up afterwards.
        if column_only && space.extent_count() == 0 {
            let live: Vec<bool> = membership.live_slots();
            let index = TileIndex::of_bytes(tessera_store::membership::pack_tile_index(
                total_rows(space),
                &column.extents(&live),
            ));
            membership.hold_no_rows();
            return Some(ArtifactRows {
                records,
                membership,
                index,
                partition: None,
                layout: ServingLayout::ArtifactMajor,
                column: None,
                base_rows: space.base_rows(),
                covered: covered_by(space),
                inherited: Vec::new(),
            });
        }
        // [`Self::build_over`]'s rule for an offered index, and its reason: a shorter one leaves
        // every ordinal past its end out of every walk.
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
            records,
            membership,
            index,
            partition: None,
            layout: ServingLayout::ArtifactMajor,
            column: None,
            base_rows: space.base_rows(),
            covered: covered_by(space),
            inherited: Vec::new(),
        })
    }

    /// The same family over a membership **resolved elsewhere** — a spatial level's, joined from
    /// the per-segment pieces the flush resolved (`crate::shapes`), in this generation's whole row
    /// space rather than its base.
    ///
    /// `rows` is parallel to the level's ordinals and is the per-row source; the records supply
    /// everything else, exactly as [`Self::build_over`] reads them. `row_count` is the generation's
    /// total row count — base and every extent — because a shape's membership covers a flushed
    /// row the moment its segment publishes, which is what the tile index and the column below
    /// must be sized to.
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
            records,
            membership,
            index,
            partition: None,
            layout: ServingLayout::ArtifactMajor,
            column: None,
            base_rows: space.base_rows(),
            covered: covered_by(space),
            inherited: Vec::new(),
        }
    }

    /// Serve this level row-major, from `column`.
    ///
    /// **`None` puts the level back on the artifact-major route and records that**, which is the one
    /// place the *recorded* layout and the *served* one are allowed to differ: a level whose
    /// memberships turned out to overlap, or whose fold-written file would not open, has no column
    /// to scan, and the row form beside it answers every question the column would have. The trace
    /// is at the call site, where the reason is known.
    pub fn with_column(mut self, column: Option<Arc<RowColumn>>) -> Self {
        self.layout = column
            .as_ref()
            .map(|column| column.layout())
            .unwrap_or(ServingLayout::ArtifactMajor);
        self.column = column;
        self
    }

    /// Attach the containment partition composed for the same level at the same level version.
    ///
    /// Separate from [`Self::build`] because the two read different things — this one reads the
    /// postings, which no row form needs — and because a caller that cannot supply a partition
    /// (a foreign plugin, or a probe measuring the masked-count route) must be able to build the
    /// form without one.
    pub fn with_partition(mut self, partition: Option<ContainmentPartition>) -> Self {
        self.partition = partition;
        self
    }

    /// Row-space memberships and a serving column, with no `RowSpace` to project through — for
    /// the membership column's own tests, which are about the resolver's two routes and not the
    /// projection.
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
            records: ArtifactRecords {
                attachments: vec![None; sets.len()],
                parents: vec![Vec::new(); sets.len()],
                declared: vec![Vec::new(); sets.len()],
            },
            membership,
            index,
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
    /// target's membership (decision 0145, `annotations.md` §2.2).
    ///
    /// A label with no member rows is the label of its cluster. It sits in the tiles the cluster
    /// sits in, its masked count is the cluster's masked count, its existence criterion reads that
    /// number, and its proportional denominator is the cluster's declared size. A label that
    /// declares members keeps them: they are the generating set the caller claimed (decision 0135),
    /// and `content_requires = "all"` gates on them unchanged.
    ///
    /// The rule is the store's, in
    /// [`tessera_lifecycle::membership::ArtifactStore::members_of`], which the build's artifact
    /// pass and the fold's read as well (decision 0139). What this function owns is where the rule
    /// is applied: on the level's row form, once, so the tile index, the masked count, the
    /// criterion, the declared size and every derived property follow from one membership. Every
    /// route reads that form: the viewport, the drill-down, a filter, the dependency prerequisite.
    ///
    /// A borrowing artifact gains nothing its target's gate would withhold. The membership is a set
    /// of rows, and every count taken over it is taken against this viewer's own composed mask
    /// (**I2**), so it admits no member the viewer's mask does not already admit. Existence is the
    /// target's too, one conjunct earlier: an attached artifact is absent wherever its target is
    /// absent, on the target's whole predicate and on every route ([`ArtifactView::verdict`] step
    /// 3, decision 0089). A principal not served the cluster is not served its label, filtered or
    /// not (**I3**, **I12**), and a filter moves neither number.
    ///
    /// The membership is the target's as it stands now. The versions borrowed from are recorded in
    /// [`Self::inherited`], so a target that grows re-derives its labels at the next request that
    /// finds the form ([`Self::inherited_current`]).
    ///
    /// The form is served artifact-major once anything borrows. A build and a fold write such a
    /// level's column over the borrowed membership as it stood then (they read
    /// [`tessera_lifecycle::membership::ArtifactStore::members_of`] too), and that is a set the
    /// target's version moves without moving this level's, so the column is dropped here rather
    /// than served from or amended. The level's own bitmaps are cheap: a label level holds one
    /// artifact per cluster.
    ///
    /// Returns how many ordinals took a membership that is not their own.
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
            // **The store's own rule, not a second walk** (decision 0139): the build's artifact
            // pass and the fold's read the same function, so what a bundle's tile index describes
            // and what this form counts cannot come apart. The hops are what this form records so
            // it can tell when what it borrowed has moved.
            let mut hops = Vec::new();
            let members = store.members_of_tracked(record, &mut hops);
            if hops.is_empty() {
                // Nothing resolved: a hole, or an ordinal holding another entity. The artifact
                // keeps the empty membership it declared, which is a count of zero for everyone.
                continue;
            }
            if taken == 0 && !self.membership.rows_held() {
                // A column-only form holds no bitmap to overwrite. The level is rebuilt
                // artifact-major from its records first, which is the same recovery the alarm
                // below [`ArtifactProjections::get_or_build`]'s column branch makes.
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
            self.index = TileIndex::build(&self.membership, total_rows(space));
        }
        self.inherited = borrowed;
        taken
    }

    /// Whether every membership this form borrowed is still the membership it borrowed
    /// ([`Self::inherited`]). True for a form that borrowed nothing, which is every ordinary level.
    pub(super) fn inherited_current(&self, store: &ArtifactStore) -> bool {
        self.inherited
            .iter()
            .all(|(layer, level, version)| store.level_version(layer, *level) == *version)
    }
}
