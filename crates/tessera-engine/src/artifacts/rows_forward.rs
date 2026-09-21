//! Bringing a held row form forward across a growth, a publication, a flush and a merge.

use std::sync::Arc;

use croaring::Bitmap;

use tessera_lifecycle::membership::ArtifactRecord;
use tessera_types::layer::ServingLayout;

use tessera_store::permutation::RowSpace;

use crate::tile_index::TileIndex;

use super::*;

impl ArtifactRows {
    /// Whether this form's segments and `space`'s agree as far as the shorter of the two goes — the
    /// base row count equal, and one extent list a prefix of the other. `seg_id`s are never reused,
    /// so ids standing where they stood are the same segments and the same rows even after a merge.
    pub(super) fn agrees_with(&self, space: &RowSpace) -> bool {
        if self.base_rows != space.base_rows() {
            return false;
        }
        let shared = self.covered.len().min(space.extent_count());
        self.covered[..shared]
            .iter()
            .zip(&space.extents()[..shared])
            .all(|(held, extent)| held == &extent.seg_id)
    }

    /// Whether this form may answer for `space` — the check read beside [`ProjectionKey`] at every
    /// cache hit. The form must hold at least every extent `space` carries, on the agreeing prefix,
    /// or it would understate every artifact with a member in the segments it is missing. Longer is
    /// served, and that is not laxity: a flush appends, so extra rows above `space`'s whole row
    /// count cannot collide with a row `space` addresses, and a session one geometry behind the
    /// newest reads the same form rather than rebuilding it for ever. A form a request built
    /// against a generation superseded while it built is dropped with a `warn` here.
    pub fn covers(&self, space: &RowSpace) -> bool {
        self.covered.len() >= space.extent_count() && self.agrees_with(space)
    }

    /// Whether [`Self::extend_by`] over `space` would be exact — whether `space` appends to the row
    /// space this form holds rather than permuting it. [`Self::covers`] read the other way round.
    pub(super) fn extends_to(&self, space: &RowSpace) -> bool {
        self.covered.len() <= space.extent_count() && self.agrees_with(space)
    }

    pub(super) fn covering(&mut self, space: &RowSpace) {
        self.base_rows = space.base_rows();
        self.covered = covered_by(space);
    }

    /// The rows an accepted growth adds, unioned into the ordinal that grew — the delta that keeps
    /// this form current instead of one the next request has to build again. `joining` is entity
    /// space and is projected through the whole of `space`. An entity still in the commit buffer
    /// has no row and projects to nothing; it reaches the form at its flush, through
    /// [`Self::extend_by`]. A hole takes nothing.
    pub(super) fn grow_rows(&mut self, ordinal: u32, joining: &Bitmap, space: &RowSpace) -> Bitmap {
        let rows = space.project(joining);
        let held = self.membership.get(ordinal).cloned().unwrap_or_default();
        // What this row form did not already hold. A column-only form holds none, so every
        // projected row is offered; `RowColumn::amend` skips a pair it already carries.
        let fresh = rows.andnot(&held);
        self.membership.or_rows(ordinal as usize, &rows);
        fresh
    }

    /// One newly published artifact placed at its ordinal, exactly as [`Self::build_over`]'s walk
    /// would have placed it. A publication only ever appends ordinals, so this widens the form.
    pub(super) fn publish_at(
        &mut self,
        ordinal: u32,
        record: &ArtifactRecord,
        space: &RowSpace,
    ) -> Arc<Bitmap> {
        let idx = ordinal as usize;
        Arc::make_mut(&mut self.records).put(idx, record);
        self.membership.put(idx, record, space)
    }

    /// [`Self::publish_at`] for a membership resolved elsewhere — a spatial level's new shape.
    pub(super) fn publish_resolved(
        &mut self,
        ordinal: u32,
        record: &ArtifactRecord,
        rows: Bitmap,
        space: &RowSpace,
    ) -> Arc<Bitmap> {
        let idx = ordinal as usize;
        Arc::make_mut(&mut self.records).put(idx, record);
        self.membership.put_resolved(idx, record, rows, space)
    }

    /// One generating set unioned with the entities a page joined to it — the fast arm of the
    /// tick's publication, for a page holding no leave. `false` where the ordinal is a hole or
    /// holds no content at that rank.
    pub(super) fn grow_generating(
        &mut self,
        ordinal: u32,
        rank: u16,
        joining: &Bitmap,
        space: &RowSpace,
    ) -> bool {
        let Some(sets) = self.membership.generating.get_mut(ordinal as usize) else {
            return false;
        };
        let Some(set) = sets.get_mut(rank as usize) else {
            return false;
        };
        set.or_inplace(&space.project(joining));
        true
    }

    /// One artifact's records entry read again, and its operators re-derived where a page held a
    /// leave. The operators are re-projected from entity truth where `whole` says so; the
    /// membership is untouched.
    pub(super) fn refresh_sets(
        &mut self,
        ordinal: u32,
        record: &ArtifactRecord,
        space: &RowSpace,
        whole: bool,
    ) {
        let idx = ordinal as usize;
        if idx >= self.records.len() {
            return;
        }
        Arc::make_mut(&mut self.records).put(idx, record);
        if whole {
            self.membership.project_generating(idx, record, space);
        }
    }

    /// Every artifact's membership extended by the extents this form does not yet cover — what a
    /// flush does to a stored level's held form: disjoint from everything already held, because an
    /// extent's rows begin exactly where row space ended. Callers check [`Self::extends_to`] first;
    /// this does not, so re-deriving it here is not a second place to get that answer wrong.
    pub(super) fn extend_by<'a>(
        &mut self,
        artifacts: impl Iterator<Item = (u32, &'a ArtifactRecord)>,
        space: &RowSpace,
    ) -> (Vec<(u32, u32)>, u64) {
        let from = self.covered.len();
        let mut added = Vec::new();
        let mut taken = 0u64;
        for (ordinal, record) in artifacts {
            self.extend_generating_of(ordinal, record, space, from);
            let rows = space.project_extents_from(&record.members, from);
            if rows.is_empty() {
                continue;
            }
            if self.membership.or_rows(ordinal as usize, &rows) {
                taken += rows.cardinality();
                if self.layout.is_row_major() {
                    added.extend(rows.iter().map(|row| (row, ordinal)));
                }
            }
        }
        (added, taken)
    }

    /// Every artifact's generating sets extended by the extents this form does not yet cover —
    /// [`Self::extend_by`]'s generating half, for routes whose memberships come from a segment's
    /// resolution rather than from the records ([`Self::extend_by_resolved`]).
    fn extend_generating<'a>(
        &mut self,
        artifacts: impl Iterator<Item = (u32, &'a ArtifactRecord)>,
        space: &RowSpace,
    ) {
        let from = self.covered.len();
        for (ordinal, record) in artifacts {
            self.extend_generating_of(ordinal, record, space, from);
        }
    }

    /// One artifact's generating sets extended by the extents at or after `from`.
    fn extend_generating_of(
        &mut self,
        ordinal: u32,
        record: &ArtifactRecord,
        space: &RowSpace,
        from: usize,
    ) {
        for (rank, content) in record.contents.iter().enumerate() {
            let rows = space.project_extents_from(&content.generated_from, from);
            if !rows.is_empty() {
                self.membership.or_generating(ordinal as usize, rank, &rows);
            }
        }
    }

    /// [`Self::extend_by`] for a spatial level: `piece` is the new segment's resolution, taken at
    /// `row_base`. A hole takes nothing. The generating sets are extended here too, or a served
    /// content's set would project short of the size its record declares.
    pub(super) fn extend_by_resolved<'a>(
        &mut self,
        piece: &[Option<Bitmap>],
        artifacts: impl Iterator<Item = (u32, &'a ArtifactRecord)>,
        space: &RowSpace,
        row_base: u32,
    ) -> (Vec<(u32, u32)>, u64) {
        self.extend_generating(artifacts, space);
        let mut added = Vec::new();
        let mut taken = 0u64;
        for (ordinal, part) in piece.iter().enumerate() {
            let Some(part) = part.as_ref().filter(|part| !part.is_empty()) else {
                continue;
            };
            let rows = part.add_offset(i64::from(row_base));
            if self.membership.or_rows(ordinal, &rows) {
                taken += rows.cardinality();
                if self.layout.is_row_major() {
                    added.extend(rows.iter().map(|row| (row, ordinal as u32)));
                }
            }
        }
        (added, taken)
    }

    /// [`Self::rebase_span`] for a spatial level: `piece` is the merged segment's resolution,
    /// taken at the span's start.
    pub(super) fn rebase_span_resolved<'a>(
        &mut self,
        piece: &[Option<Bitmap>],
        artifacts: impl Iterator<Item = (u32, &'a ArtifactRecord)>,
        space: &RowSpace,
        start: usize,
    ) -> (u32, u32, Vec<(u32, u32)>, u64) {
        self.rebase_generating(artifacts, space, start);
        let extent = &space.extents()[start];
        let lo = extent.row_base;
        let hi = lo.saturating_add(extent.row_count());
        let mut added = Vec::new();
        let mut taken = 0u64;
        for (ordinal, part) in piece.iter().enumerate() {
            let rows = match part {
                Some(part) => part.add_offset(i64::from(lo)),
                None => continue,
            };
            if self.membership.rebase_rows(ordinal, lo, hi, &rows) {
                taken += rows.cardinality();
                if self.layout.is_row_major() {
                    added.extend(rows.iter().map(|row| (row, ordinal as u32)));
                }
            }
        }
        (lo, hi, added, taken)
    }

    /// Carry the two derived structures over the amendment — the tile index, re-derived, and the
    /// row-major column, amended at `added` and nowhere else. The asymmetry is what each costs: an
    /// extent is `minimum`/`maximum` per artifact, O(1) a bitmap, where a column is one entry per
    /// membership entry, measured at scale in the tens of seconds on the executor thread, blocking
    /// every ingest and every deny. So the column takes the delta instead, amended in place.
    ///
    /// Neither is re-adopted from the prefix: the fold's files describe the level as it was before
    /// the amendment, and a narrow extent settles an artifact whose members reach outside the
    /// viewport. `true` where a level that was served row-major no longer has a column: a growth
    /// can make two memberships overlap, which a label column cannot express, and the level then
    /// serves artifact-major, answering identically.
    pub(super) fn amend_derived(&mut self, added: &[(u32, u32)], row_count: u32) -> bool {
        self.derived_over(None, added, row_count)
    }

    /// [`Self::amend_derived`] and [`Self::rebase_derived`]'s one walk: the tile index, and the
    /// column amended at `added` — over a `span` the merge's rebase gives up first.
    fn derived_over(
        &mut self,
        span: Option<(u32, u32)>,
        added: &[(u32, u32)],
        row_count: u32,
    ) -> bool {
        // A column-only form has no row form to re-derive from, so the extents take the same delta
        // the column does: widening by `added` is exact where rows are only added ([`TileIndex::amend`]).
        if self.membership.rows_held() {
            self.index = Arc::new(TileIndex::build(&self.membership, row_count));
        } else {
            Arc::make_mut(&mut self.index).amend(added, self.membership.len() as u32, row_count);
        }
        let Some(column) = &mut self.column else {
            return false;
        };
        let took = match span {
            Some((lo, hi)) => Arc::make_mut(column).rebase(lo, hi, added, row_count),
            None => Arc::make_mut(column).amend(added, row_count),
        };
        if took {
            return false;
        }
        self.lose_column();
        true
    }

    /// Every artifact's membership rebased over the extent at `start` — what a row-space merge does
    /// to a held form. The merged extent stands where the run it consumed stood, at the same
    /// `row_base` with the same row count, and the rows inside it are the consumed segments' rows
    /// in another order; so each artifact's bits in that span are cleared and its members
    /// re-projected through the one extent. Rows below the span are base rows or earlier extents'
    /// rows, which a merge does not move; rows above it belong to later extents, whose `row_base` a
    /// merge preserves. Returns `(lo, hi, added)`: the span cleared and the `(row, ordinal)` pairs
    /// the column takes back, on [`Self::extend_by`]'s terms.
    pub(super) fn rebase_span<'a>(
        &mut self,
        artifacts: impl Iterator<Item = (u32, &'a ArtifactRecord)>,
        space: &RowSpace,
        start: usize,
    ) -> (u32, u32, Vec<(u32, u32)>, u64) {
        let extent = &space.extents()[start];
        let lo = extent.row_base;
        let hi = lo.saturating_add(extent.row_count());
        let mut added = Vec::new();
        let mut taken = 0u64;
        for (ordinal, record) in artifacts {
            Self::rebase_generating_of(&mut self.membership, ordinal, record, space, start, lo, hi);
            let rows = space.project_extent(&record.members, start);
            if self.membership.rebase_rows(ordinal as usize, lo, hi, &rows) {
                taken += rows.cardinality();
                if self.layout.is_row_major() {
                    added.extend(rows.iter().map(|row| (row, ordinal)));
                }
            }
        }
        (lo, hi, added, taken)
    }

    /// Every artifact's generating sets rebased over the extent at `start` —
    /// [`Self::rebase_span`]'s generating half, for the route whose memberships come from a
    /// segment's resolution ([`Self::rebase_span_resolved`]).
    fn rebase_generating<'a>(
        &mut self,
        artifacts: impl Iterator<Item = (u32, &'a ArtifactRecord)>,
        space: &RowSpace,
        start: usize,
    ) {
        let extent = &space.extents()[start];
        let lo = extent.row_base;
        let hi = lo.saturating_add(extent.row_count());
        for (ordinal, record) in artifacts {
            Self::rebase_generating_of(&mut self.membership, ordinal, record, space, start, lo, hi);
        }
    }

    /// One artifact's generating sets rebased over the merged extent at `start`.
    fn rebase_generating_of(
        membership: &mut MembershipRows,
        ordinal: u32,
        record: &ArtifactRecord,
        space: &RowSpace,
        start: usize,
        lo: u32,
        hi: u32,
    ) {
        for (rank, content) in record.contents.iter().enumerate() {
            let rows = space.project_extent(&content.generated_from, start);
            membership.rebase_generating(ordinal as usize, rank, lo, hi, &rows);
        }
    }

    /// [`Self::amend_derived`] for a rebase: the tile index re-derived, the column's labels in
    /// `lo..hi` given up and `added` taken in their place ([`RowColumn::rebase`]).
    pub(super) fn rebase_derived(&mut self, lo: u32, hi: u32, added: &[(u32, u32)], row_count: u32) -> bool {
        self.derived_over(Some((lo, hi)), added, row_count)
    }

    /// The refused amendment's posture, on a form that holds its own bitmaps: the level goes back
    /// to the artifact-major route. A form that holds no bitmaps keeps its column here — there is
    /// nothing to fall back to — and the caller recomposes it as a list ([`Self::recompose_as_list`]).
    fn lose_column(&mut self) {
        if self.membership.rows_held() {
            self.layout = ServingLayout::ArtifactMajor;
            self.column = None;
        }
    }

    /// Take the list form from the column this level already holds, after an amendment the label
    /// form could not express — see [`RowColumn::recompose_as_list`]. The extents are re-derived
    /// from the new column's own bytes, exactly as they were at the build: the form's one
    /// membership is the column, and the two may not be allowed to disagree. `false` where the
    /// composition failed, an I/O failure and not a shape: the caller drops the form and the next
    /// request projects the level whole.
    pub(super) fn recompose_as_list(&mut self, added: &[(u32, u32)], scratch: &std::path::Path) -> bool {
        let Some(column) = self.column.as_deref() else {
            return false;
        };
        let row_count = self.index.row_count().max(column.row_count());
        let Some(listed) = column.recompose_as_list(added, self.base_rows, row_count, scratch)
        else {
            return false;
        };
        let live = self.membership.live_slots();
        self.index = Arc::new(TileIndex::of_bytes(
            tessera_store::membership::pack_tile_index(row_count, &listed.extents(&live)),
        ));
        self.layout = listed.layout();
        self.column = Some(Arc::new(listed));
        true
    }
}
