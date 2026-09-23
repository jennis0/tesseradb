//! The row-space forms an annotation layer's memberships take, and what they answer.

use std::sync::Arc;

use croaring::Bitmap;

use tessera_lifecycle::membership::{ArtifactRecord, ArtifactStore, Attachment};
use tessera_lifecycle::wal::ParentRef;
use tessera_types::layer::ServingLayout;

use tessera_store::permutation::RowSpace;

use crate::compose::MaskedSet;
use crate::containment::{ContainmentAnswers, ContainmentPartition};
use crate::row_column::RowColumn;
use crate::tile_index::TileIndex;

/// One level's per-ordinal facts that no row space is involved in: the attachment edge, the
/// parent edge, and each content's declared generating-set size.
///
/// Split from [`MembershipRows`] because the membership half decodes and projects every
/// membership, where this is a walk copying three small things per artifact. Everything `verdict`
/// asks that is not a masked question is answered from here. The generating sets stay in the
/// registry rather than a second copy here, and
/// [`ArtifactView::declared_size`](super::view::ArtifactView::declared_size) takes the declared size from the row form so the
/// numerator and denominator come from one projection.
#[derive(Debug, Clone, Default)]
pub struct ArtifactRecords {
    /// Per ordinal, what this artifact is an attachment to — `None` for an ordinary artifact.
    /// Entity space, not projected: a target is tested on its disposition and its layer's gate.
    pub(super) attachments: Vec<Option<Attachment>>,
    /// Per ordinal, this artifact's parent edges as the registry holds them — ascending, empty at
    /// a root, several on a `dag` layer. Not a visibility term: see [`ArtifactRecord::parents`].
    pub(super) parents: Vec<Vec<ParentRef>>,
    /// Per ordinal, per rank: `|G|` in entity space, from the durable record. Kept beside the
    /// projected set because without it a lossy projection would read as containment of a set
    /// smaller than the caller declared.
    pub(super) declared: Vec<Vec<u64>>,
    /// Per ordinal, the artifact's own access label as descriptors. Grown only as far as the last
    /// labelled ordinal, so a layer whose artifacts carry no label holds nothing here.
    pub(super) access: Vec<Option<Arc<[Vec<u8>]>>>,
}

/// One layer's membership in the row space of one view, built at open and rebuilt when the
/// generation moves — the expensive half of [`ArtifactRows`]. Built member-wise, never
/// range-wise: projecting an entity range to a row range would admit whatever documents sit
/// between two members in Morton order, and one extra member can lift an artifact over its
/// existence criterion.
#[derive(Debug, Clone)]
pub struct MembershipRows {
    /// Parallel to a level's ordinals; `None` is a hole, not an empty membership. One `Arc` per
    /// artifact, so `Arc::make_mut` on a write copies only the bitmap that grew.
    pub(super) rows: Vec<Option<Arc<Bitmap>>>,
    /// Per ordinal, per rank: that content's generating set in row space. Pushed in lockstep with
    /// [`ArtifactRecords::declared`], the size the same set had in entity space.
    pub(super) generating: Vec<Vec<Bitmap>>,
    /// Whether [`Self::rows`] holds anything at all. `false` on a level served row-major from a
    /// column: the column is the membership addressed by row, each artifact's extent is folded out
    /// of the column's own bytes ([`RowColumn::extents`]), and nothing is transposed back into an
    /// artifact-major bitmap per ordinal. [`Self::get`] then answers `None` for every ordinal, so a
    /// reader wanting one artifact's rows is told they are not held rather than handed an empty
    /// set — see [`ArtifactRows::visible_rows`].
    pub(super) rows_held: bool,
}

/// One level's row form and the records beside it, under one validity key.
///
/// One snapshot: the records, the projection, the tile index and the containment partition are all
/// built from a single borrow of the [`ArtifactStore`] at a single level version, so a write
/// landing between two reads cannot leave one of them describing a population the others no longer
/// have — a membership that grew between two reads would leave the extent beside it narrow, and a
/// narrow extent settles an artifact whose members reach outside the viewport.
#[derive(Debug, Clone, Default)]
pub struct ArtifactRows {
    /// **`Arc`, because a flush clones this form and never writes here.** A publication takes
    /// `Arc::make_mut` on the whole form to extend one level's membership; the entity-space half
    /// is untouched by that, and a deep copy of it is three vectors per artifact. That copy and
    /// [`Self::index`]'s together were 82 ms a publication on a level holding a quarter of a
    /// million admin divisions. The writers, [`ArtifactRows::put`] and its siblings, copy it then.
    pub(super) records: Arc<ArtifactRecords>,
    pub(super) membership: MembershipRows,
    /// The hierarchical row-range index and the per-artifact extents — always present, because the
    /// walk is how candidacy is answered. A level with no artifacts has an empty one.
    ///
    /// **`Arc` for the same reason as [`Self::records`], and more sharply**: a flush's publication
    /// rebuilds this wholesale a moment after cloning it ([`ArtifactRows::amend_derived`]), so the
    /// copy of its per-node bitmaps was thrown away every time.
    pub(super) index: Arc<TileIndex>,
    /// The containment partition, where this level has one. `None` under any plugin but the
    /// builtin — see [`crate::containment`] — and containment then stays on the masked-count route.
    pub(super) partition: Option<ContainmentPartition>,
    /// Which form this level is served in, and the row-addressed column where that form has one.
    /// A level recorded row-major whose column would not compose is served artifact-major, and
    /// this field says so.
    pub(super) layout: ServingLayout,
    pub(super) column: Option<Arc<RowColumn>>,
    /// The row space this form's memberships were projected through — the base row count, and the
    /// `seg_id` of every extent whose rows are in them, in order. A merge renumbers extent rows, so
    /// a form that held them without recording which segments they came from would serve one
    /// segment's rows as another's after a merge. The whole list, not a count and a boundary id,
    /// because this form may be asked about by requests at two live generations at once; comparing
    /// the shared prefix answers both, exact because `seg_id`s are never reused.
    pub(super) base_rows: u32,
    pub(super) covered: Vec<String>,
    /// What this form borrowed and the version it borrowed it at: one entry per distinct
    /// `(layer, level)` an artifact of this level took its membership from, because it declared
    /// none of its own ([`ArtifactRows::inherit`]). Empty on a level that borrows nothing.
    ///
    /// A target that grows moves the target's version and not this level's, so without this a
    /// label of a cluster that gained members would go on answering over the membership the
    /// cluster had when the label's form was built. Read at every cache hit
    /// ([`ArtifactRows::inherited_current`]) beside [`ArtifactRows::covers`].
    pub(super) inherited: Vec<(String, u32, u64)>,
}

/// What one viewport's narrowing produced, on whichever route the level's layout takes. Neither
/// variant is a verdict: [`ArtifactView::verdict`](super::ArtifactView::verdict) runs for every ordinal either hands back.
pub enum Candidacy {
    /// The artifact-major route: the tile index's walk, with the settled half carried so a probe
    /// can be exact for the mask-shaped question too.
    Indexed(crate::tile_index::Candidates),
    /// The row-major route: one scan of `viewport ∩ M_auth` marking labels. Every ordinal here has
    /// already paid its masked probe, since the scan was over the visible rows.
    Scanned(Bitmap),
}

/// What a filtered request's hoisted `viewport ∩ M_auth ∩ M_sel` produced, on whichever route the
/// level's layout takes — see [`ArtifactRows::matched`]. Neither variant admits or withholds an
/// artifact: existence and the masked count are anchored on `M_auth`, filter or no filter.
pub enum Matched<'a> {
    /// The answer for the whole level, one pass over the matched set.
    Scanned(Bitmap),
    /// The matched set itself, borrowed, for the artifact-major route: one early-exiting probe per
    /// served artifact, nothing materialised per level.
    PerArtifact(&'a Bitmap),
}

impl Candidacy {
    /// Every candidate ordinal, ascending.
    pub fn iter(&self) -> Box<dyn Iterator<Item = u32> + '_> {
        match self {
            Candidacy::Indexed(candidates) => Box::new(candidates.iter()),
            Candidacy::Scanned(rows) => Box::new(rows.iter()),
        }
    }

    pub fn len(&self) -> u64 {
        match self {
            Candidacy::Indexed(candidates) => candidates.len(),
            Candidacy::Scanned(rows) => rows.cardinality(),
        }
    }

    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }
}

/// The containment test's three outcomes. `NothingToContain` and `Unsatisfied` are not the same
/// answer: the first is a layer declaring no supplied content, whose artifacts serve on their
/// other conjuncts; the second is an artifact with a description this viewer may not read, and is
/// therefore absent.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Containment {
    /// The artifact carries no supplied content.
    NothingToContain,
    /// The viewer contains the generating set at this rank entirely, and is served it whole.
    Satisfied(u32),
    /// The viewer contains no content's generating set.
    Unsatisfied,
}

impl ArtifactRecords {
    /// Walk a level's records for the three per-ordinal facts that need no row space. Cheap: no
    /// membership is decoded and nothing is projected.
    pub fn build<'a>(artifacts: impl Iterator<Item = (u32, &'a ArtifactRecord)>) -> Self {
        let mut records = ArtifactRecords::default();
        for (ordinal, record) in artifacts {
            records.put(ordinal as usize, record);
        }
        records
    }

    /// Place one record at `idx`, growing the dense vectors to reach it. A slot never written is a
    /// hole, not an empty artifact — see [`ArtifactStore`].
    pub(super) fn put(&mut self, idx: usize, record: &ArtifactRecord) {
        if self.attachments.len() <= idx {
            self.attachments.resize_with(idx + 1, || None);
            self.parents.resize_with(idx + 1, Vec::new);
            self.declared.resize_with(idx + 1, Vec::new);
        }
        self.attachments[idx] = record.attached_to.clone();
        self.parents[idx] = record.parents.clone();
        // The set's stored cardinality, moved by the page that joined or left it, not a count
        // taken of the set here: the store refuses a page whose count disagrees.
        self.declared[idx] = record.contents.iter().map(|v| v.cardinality).collect();
        if !record.access.is_empty() {
            if self.access.len() <= idx {
                self.access.resize_with(idx + 1, || None);
            }
            self.access[idx] = Some(Arc::from(record.access.as_slice()));
        } else if let Some(slot) = self.access.get_mut(idx) {
            *slot = None;
        }
    }

    /// The artifact's own access label as descriptors, empty where it carries none.
    pub(crate) fn access(&self, ordinal: u32) -> &[Vec<u8>] {
        self.access
            .get(ordinal as usize)
            .and_then(Option::as_deref)
            .unwrap_or(&[])
    }

    /// What the artifact at `ordinal` hangs from, if it hangs from anything.
    pub(crate) fn attachment(&self, ordinal: u32) -> Option<&Attachment> {
        self.attachments
            .get(ordinal as usize)
            .and_then(Option::as_ref)
    }

    /// The artifact's parent edges, as the registry holds them — ascending by ordinal, empty at
    /// a root and at a hole. The only read the engine makes of the record's parents.
    pub(crate) fn parents(&self, ordinal: u32) -> &[ParentRef] {
        self.parents
            .get(ordinal as usize)
            .map_or(&[], Vec::as_slice)
    }

    /// Public for the differential: a containment test reads the operator and this number
    /// together, and a test comparing only the operator would pass a form whose cardinalities came
    /// from another version of the set.
    pub fn declared_sizes(&self, ordinal: u32) -> &[u64] {
        self.declared(ordinal)
    }

    /// `|G|` per rank, entity space. Empty for a hole and for an artifact with no contents alike —
    /// the two are told apart by [`ArtifactRows::satisfied_rank`], never here.
    fn declared(&self, ordinal: u32) -> &[u64] {
        self.declared
            .get(ordinal as usize)
            .map(Vec::as_slice)
            .unwrap_or(&[])
    }

    /// How many ordinals this level covers, holes included.
    pub fn len(&self) -> usize {
        self.attachments.len()
    }

    pub fn is_empty(&self) -> bool {
        self.attachments.is_empty()
    }
}

impl Default for MembershipRows {
    fn default() -> Self {
        MembershipRows {
            rows: Vec::new(),
            generating: Vec::new(),
            rows_held: true,
        }
    }
}

impl MembershipRows {
    /// Project a level's memberships into `space`. Costly and not on any per-request path:
    /// `RowSpace::project` decodes the whole membership. Paid at open and at a generation move.
    pub fn build<'a>(
        artifacts: impl Iterator<Item = (u32, &'a ArtifactRecord)>,
        space: &RowSpace,
    ) -> Self {
        let mut membership = MembershipRows::default();
        for (ordinal, record) in artifacts {
            membership.put(ordinal as usize, record, space);
        }
        membership
    }

    /// Returns the rows it projected, which the publication arms need whether or not this form
    /// keeps them ([`Self::rows_held`]).
    pub(super) fn put(&mut self, idx: usize, record: &ArtifactRecord, space: &RowSpace) -> Arc<Bitmap> {
        if self.rows.len() <= idx {
            self.rows.resize_with(idx + 1, || None);
            self.generating.resize_with(idx + 1, Vec::new);
        }
        let rows = Arc::new(space.project(&record.members));
        // A column-only form keeps the slot and not the set: the `Arc` is shared, not the bitmap.
        self.rows[idx] = Some(if self.rows_held {
            Arc::clone(&rows)
        } else {
            Arc::new(Bitmap::new())
        });
        self.generating[idx] = record
            .contents
            .iter()
            // A member still in the commit buffer has no row, so the set projects short of its
            // declared size and [`ArtifactRows::satisfied_rank`] withholds the content from
            // everyone until the flush that gives it one.
            .map(|v| space.project(&v.generated_from))
            .collect();
        rows
    }

    /// One artifact's generating sets alone, with an empty membership standing in for rows a
    /// transposed column supplies afterwards ([`Self::absorb_transposed`]). The generating sets
    /// still project: a row column holds membership and nothing else.
    pub(super) fn put_generating(&mut self, idx: usize, record: &ArtifactRecord, space: &RowSpace) {
        if self.rows.len() <= idx {
            self.rows.resize_with(idx + 1, || None);
            self.generating.resize_with(idx + 1, Vec::new);
        }
        self.rows[idx] = Some(Arc::new(Bitmap::new()));
        self.generating[idx] = record
            .contents
            .iter()
            .map(|v| space.project(&v.generated_from))
            .collect();
    }

    /// One artifact's generating sets projected again from the record, where a page held a leave
    /// or a fill changed which contents the artifact has. The membership is left untouched.
    pub(super) fn project_generating(&mut self, idx: usize, record: &ArtifactRecord, space: &RowSpace) {
        if idx >= self.generating.len() {
            return;
        }
        self.generating[idx] = record
            .contents
            .iter()
            .map(|v| space.project(&v.generated_from))
            .collect();
    }

    /// Take a transposed column's per-ordinal rows into the slots [`Self::put_generating`] left
    /// empty. `false` where `transposed` is narrower than the level it was adopted for; the caller
    /// projects instead.
    pub(super) fn absorb_transposed(&mut self, mut transposed: Vec<Bitmap>) -> bool {
        if transposed.len() < self.rows.len() {
            return false;
        }
        for (idx, slot) in self.rows.iter_mut().enumerate() {
            if slot.is_some() {
                *slot = Some(Arc::new(std::mem::take(&mut transposed[idx])));
            }
        }
        self.rows_held = true;
        true
    }

    /// Give up the per-artifact rows and serve them from the column instead — see
    /// [`Self::rows_held`]. [`Self::absorb_transposed`] can bring the form back.
    pub(super) fn hold_no_rows(&mut self) {
        let empty = Arc::new(Bitmap::new());
        for slot in self.rows.iter_mut() {
            if slot.is_some() {
                *slot = Some(Arc::clone(&empty));
            }
        }
        self.rows_held = false;
    }

    /// One artifact whose membership rows were resolved elsewhere — a shape's — with the
    /// generating sets projected exactly as [`Self::put`] projects them.
    pub(super) fn put_resolved(
        &mut self,
        idx: usize,
        record: &ArtifactRecord,
        rows: Bitmap,
        space: &RowSpace,
    ) -> Arc<Bitmap> {
        if self.rows.len() <= idx {
            self.rows.resize_with(idx + 1, || None);
            self.generating.resize_with(idx + 1, Vec::new);
        }
        let rows = Arc::new(rows);
        self.rows[idx] = Some(if self.rows_held {
            Arc::clone(&rows)
        } else {
            Arc::new(Bitmap::new())
        });
        self.generating[idx] = record
            .contents
            .iter()
            .map(|v| space.project(&v.generated_from))
            .collect();
        rows
    }

    /// The slot at `idx` takes `rows` outright. The one caller is [`ArtifactRows::inherit`], where
    /// the rows are not this artifact's own. A membership is never otherwise replaced: it grows
    /// ([`Self::or_rows`]) or it is rebased over one extent ([`Self::rebase_rows`]).
    pub(super) fn put_rows(&mut self, idx: usize, rows: Bitmap) {
        if self.rows.len() <= idx {
            self.rows.resize_with(idx + 1, || None);
            self.generating.resize_with(idx + 1, Vec::new);
        }
        self.rows[idx] = Some(Arc::new(rows));
        self.rows_held = true;
    }

    /// Union `rows` into the slot at `idx` — the only way a held form's membership grows. A slot
    /// holding `None` is a hole and stays one: giving it rows would resurrect an artifact a fold
    /// retired under an identity a caller's `tessera_id` still names. `false` says nothing was done.
    pub(super) fn or_rows(&mut self, idx: usize, rows: &Bitmap) -> bool {
        match self.rows.get_mut(idx) {
            // A column-only form holds the slot and not the set.
            Some(Some(_)) if !self.rows_held => true,
            Some(Some(held)) => {
                // One artifact's bitmap copied where a reader holds it, never the level's.
                Arc::make_mut(held).or_inplace(rows);
                true
            }
            _ => false,
        }
    }

    /// Replace the slot's rows in `lo..hi` with `rows` — the one way a held form's membership
    /// changes without growing; a merge asks for it, since rows inside a merged span name other
    /// entities afterwards. A hole stays a hole, and an artifact untouched by the span returns
    /// early rather than paying `make_mut` on a shared form.
    pub(super) fn rebase_rows(&mut self, idx: usize, lo: u32, hi: u32, rows: &Bitmap) -> bool {
        match self.rows.get_mut(idx).and_then(Option::as_mut) {
            Some(_) if !self.rows_held => true,
            Some(held) => {
                if rows.is_empty() && held.range_cardinality(lo..hi) == 0 {
                    return true;
                }
                let held = Arc::make_mut(held);
                held.remove_range(lo..hi);
                held.or_inplace(rows);
                true
            }
            None => false,
        }
    }

    /// Union `rows` into the generating set at `(idx, rank)` — the generating half of
    /// [`Self::or_rows`], and the only way a held set grows. A slot with no sets takes nothing.
    /// A set is held whether or not the memberships are ([`Self::rows_held`]).
    pub(super) fn or_generating(&mut self, idx: usize, rank: usize, rows: &Bitmap) {
        if let Some(set) = self.generating.get_mut(idx).and_then(|s| s.get_mut(rank)) {
            set.or_inplace(rows);
        }
    }

    /// Replace the generating set at `(idx, rank)` inside `lo..hi` with `rows` — the generating
    /// half of [`Self::rebase_rows`], for the one operation that renumbers rows a set holds.
    pub(super) fn rebase_generating(&mut self, idx: usize, rank: usize, lo: u32, hi: u32, rows: &Bitmap) {
        if let Some(set) = self.generating.get_mut(idx).and_then(|s| s.get_mut(rank)) {
            if rows.is_empty() && set.range_cardinality(lo..hi) == 0 {
                return;
            }
            set.remove_range(lo..hi);
            set.or_inplace(rows);
        }
    }

    /// Whether any of this ordinal's generating sets holds a row at or above `base_rows`. One
    /// `maximum` a rank — O(1) a set.
    fn generating_above(&self, ordinal: u32, base_rows: u32) -> bool {
        self.generating(ordinal)
            .iter()
            .any(|set| set.maximum().is_some_and(|row| row >= base_rows))
    }

    /// One artifact's rows, or `None` where this form does not hold them — a hole, an ordinal
    /// past the level's end, or a form built column-only ([`Self::rows_held`]).
    pub fn get(&self, ordinal: u32) -> Option<&Bitmap> {
        if !self.rows_held {
            return None;
        }
        self.rows
            .get(ordinal as usize)
            .and_then(Option::as_ref)
            .map(Arc::as_ref)
    }

    pub fn rows_held(&self) -> bool {
        self.rows_held
    }

    /// Per ordinal, whether the level has a record there — a hole is `false`. Read by
    /// [`RowColumn::extents`], which cannot tell a hole from an artifact no row labels.
    pub(super) fn live_slots(&self) -> Vec<bool> {
        self.rows.iter().map(Option::is_some).collect()
    }

    /// Whether this form holds an artifact at `ordinal`: the slot exists and is not a hole. A
    /// column-only form answers this too, its live slots holding the one shared empty bitmap
    /// ([`Self::hold_no_rows`]).
    fn holds(&self, ordinal: u32) -> bool {
        self.rows.get(ordinal as usize).is_some_and(Option::is_some)
    }

    /// A row form given directly, for tests whose subject is the hierarchy over a row form rather
    /// than the projection into one. Not a route a stored membership takes.
    #[cfg(test)]
    pub(crate) fn of_rows(rows: Vec<Option<Bitmap>>) -> Self {
        MembershipRows {
            generating: vec![Vec::new(); rows.len()],
            rows: rows.into_iter().map(|set| set.map(Arc::new)).collect(),
            rows_held: true,
        }
    }

    /// The projected generating sets, per rank. Parallel to [`ArtifactRecords::declared`]. Public
    /// for the differential: a row form transposed out of a column takes these from the same
    /// projection the artifact-major route uses.
    pub fn generating(&self, ordinal: u32) -> &[Bitmap] {
        self.generating
            .get(ordinal as usize)
            .map(Vec::as_slice)
            .unwrap_or(&[])
    }

    /// How many ordinals this level covers, holes included.
    pub fn len(&self) -> usize {
        self.rows.len()
    }

    pub fn is_empty(&self) -> bool {
        self.rows.is_empty()
    }

    /// Blocks per artifact, observed over the form, and the number the automatic layout pick's
    /// threshold is expressed in. `blocks` is Roaring containers touched: close to 1.0 for a
    /// clustering, close to 100 for a scattered predicate. Holes and emptied memberships are not
    /// counted, or an emptied level would look more populous than it is.
    ///
    /// Separate from [`Self::shape`]: this one runs at every form build, so it must not allocate a
    /// second copy of the level.
    pub fn blocks_per_artifact(&self) -> f64 {
        let mut artifacts = 0u64;
        let mut blocks = 0u64;
        for ordinal in 0..self.len() as u32 {
            let Some(rows) = self.get(ordinal) else {
                continue;
            };
            if rows.is_empty() {
                continue;
            }
            artifacts += 1;
            blocks += rows.statistics().n_containers as u64;
        }
        if artifacts == 0 {
            0.0
        } else {
            blocks as f64 / artifacts as f64
        }
    }

    /// The shape the automatic layout pick reads — [`Self::blocks_per_artifact`] with the artifact
    /// count, the `everywhere` fraction and the disjointness observation. Called once per level per
    /// fold. `row_count` is the view's base row space, which the `everywhere` test needs.
    pub fn shape(&self, row_count: u32) -> crate::layout::LevelShape {
        tessera_store::derived::observe_shape(row_count, &|visit| {
            for ordinal in 0..self.len() as u32 {
                if let Some(rows) = self.get(ordinal) {
                    visit(ordinal, rows);
                }
            }
        })
    }
}

/// A view's own key — the last component of its path — which is what an artifact of a
/// group-scoped layer names: a group's several layouts over one key set draw the same artifact.
pub(crate) fn view_key(view: &str) -> &str {
    tessera_store::view_path_components(view)
        .last()
        .copied()
        .unwrap_or(view)
}

/// One artifact's record, and only where this view draws it — the single read every in-place
/// amendment makes of the store, so a held form takes a delta for its own view's artifacts alone.
///
/// `None` for a hole, and for an artifact belonging to another view of the same group: putting
/// such a record into this form would serve that view's key, its `tessera_id` and a live count to
/// a principal of this one. The projecting routes reach the same rule through
/// [`tessera_lifecycle::membership::ArtifactStore::level_in_view`], this test over a whole level.
pub(super) fn drawn_record<'a>(
    store: &'a ArtifactStore,
    layer: &str,
    level: u32,
    ordinal: u32,
    view: &str,
) -> Option<&'a ArtifactRecord> {
    store
        .drawn_in_view(layer, level, ordinal, view_key(view))
        .then(|| store.get(layer, level, ordinal))
        .flatten()
}

/// The whole of a view's row space — base and every extent — as a row count.
pub(super) fn total_rows(space: &RowSpace) -> u32 {
    u32::try_from(space.total_rows()).unwrap_or(u32::MAX)
}

/// The `seg_id` of every extent a row space carries, in order — see [`ArtifactRows::covered`].
pub(super) fn covered_by(space: &RowSpace) -> Vec<String> {
    space
        .extents()
        .iter()
        .map(|extent| extent.seg_id.clone())
        .collect()
}

impl ArtifactRows {
    /// Which form this level is served in — see [`ArtifactRows::with_column`] on why that is not
    /// always the form the manifest records.
    pub fn layout(&self) -> ServingLayout {
        self.layout
    }

    /// This level's row-addressed column, where it has one.
    pub fn column(&self) -> Option<&RowColumn> {
        self.column.as_deref()
    }

    /// Candidacy for one viewport, on whichever route this level's layout takes: which artifacts
    /// could have a member this viewer can see inside the viewport. `counts` is this level's
    /// masked-count histogram where the request already built one ([`crate::Engine::masked_counts`]).
    /// Where the viewport covers the whole mask ([`crate::tile_index::Viewport::covers_mask`]),
    /// the scan finds nothing the histogram has not already counted. The filtered question is
    /// [`Self::matched`]'s, asked of a narrower set, and keeps the scan.
    pub fn candidacy(
        &self,
        viewport: &crate::tile_index::Viewport<'_>,
        counts: Option<&crate::histogram::MaskedCounts>,
    ) -> Candidacy {
        // The column arm answers against `viewport ∩ M_auth`, exact for the masked question too;
        // the indexed arm is a candidate generator and every ordinal it returns still pays a probe.
        match &self.column {
            Some(column) => {
                if viewport.covers_mask() {
                    // The lengths must agree, or the histogram is not this column's — a sanity
                    // check on the pairing, not the disclosure defence: a mismatch only makes the
                    // entry short, losing artifacts rather than admitting them.
                    if let Some(counts) = counts.filter(|c| c.len() == column.len()) {
                        return Candidacy::Scanned(counts.populated());
                    }
                }
                Candidacy::Scanned(column.candidates(viewport.here()))
            }
            None => Candidacy::Indexed(self.index.candidates(viewport.rows())),
        }
    }

    /// Which of this level's artifacts hold a member the request's filter admits, given the
    /// hoisted `viewport ∩ M_auth ∩ M_sel`. The same two routes as [`Self::candidacy`], asked of a
    /// narrower set. The set handed in must come from
    /// [`crate::compose::EffectiveMask::matched_rows`] and nothing else, which is what keeps the
    /// answer inside `M_auth`.
    pub fn matched<'a>(&self, here_matched: &'a Bitmap) -> Matched<'a> {
        match &self.column {
            Some(column) => Matched::Scanned(column.candidates(here_matched)),
            None => Matched::PerArtifact(here_matched),
        }
    }

    /// Whether one artifact holds such a member — see [`Self::matched`]. The probe is
    /// early-exiting, so it is cheap on a hit and a full pass over the artifact's containers
    /// otherwise.
    pub fn matches(&self, matched: &Matched<'_>, ordinal: u32) -> bool {
        match matched {
            Matched::Scanned(ordinals) => ordinals.contains(ordinal),
            Matched::PerArtifact(here_matched) => self.intersects_visible(ordinal, here_matched),
        }
    }

    /// The entity-space half.
    pub fn records(&self) -> &ArtifactRecords {
        &self.records
    }

    /// The row-space half.
    pub fn membership(&self) -> &MembershipRows {
        &self.membership
    }

    /// This level's tile index, the walk that decides which artifacts a viewport asks about.
    pub fn index(&self) -> &TileIndex {
        &self.index
    }

    /// This level's containment partition, if it has one.
    pub fn partition(&self) -> Option<&ContainmentPartition> {
        self.partition.as_ref()
    }

    /// What the artifact at `ordinal` hangs from, if it hangs from anything.
    pub(crate) fn attachment(&self, ordinal: u32) -> Option<&Attachment> {
        self.records.attachment(ordinal)
    }

    /// The artifact's parent edges, as the registry holds them — see [`ArtifactRecords::parents`].
    pub(crate) fn parents(&self, ordinal: u32) -> &[ParentRef] {
        self.records.parents(ordinal)
    }

    pub fn get(&self, ordinal: u32) -> Option<&Bitmap> {
        self.membership.get(ordinal)
    }

    /// `membership ∩ M_auth` for one artifact — the rows of its membership this viewer may see.
    ///
    /// Two routes. Where the form holds per-artifact rows, it is one intersection with the mask.
    /// Where it does not — a column-only form ([`MembershipRows::rows_held`]) — it is a walk of the
    /// rows this viewer may see inside the artifact's extent, reading labels off the column. A
    /// scattered artifact's extent is the whole row space, so that walk costs as much as scanning
    /// `M_auth`: nothing that runs per served artifact may call this. Composed from inside the
    /// mask: the span is handed to [`MaskedSet::visible_rows`], the only route to a visible row set.
    pub fn visible_rows(&self, ordinal: u32, mask: &impl MaskedSet) -> Bitmap {
        if let Some(rows) = self.get(ordinal) {
            return mask.visible_rows(rows);
        }
        let Some(column) = self.column() else {
            return Bitmap::new();
        };
        let crate::tile_index::Extent::Span { lo, hi } = self.index.extent(ordinal) else {
            return Bitmap::new();
        };
        // Materialised before it is walked, so a scattered artifact costs a copy of `M_auth` on
        // top of the walk: [`MaskedSet::visible_rows`] is deliberately the only route out.
        let visible = mask.visible_rows(&Bitmap::from_range(lo..=hi));
        let mut out = Bitmap::new();
        for row in visible.iter() {
            column.for_each_label(row, |labelled| {
                if labelled == ordinal {
                    out.add(row);
                }
            });
        }
        out
    }

    /// How many ordinals this level covers, holes included.
    pub fn len(&self) -> usize {
        self.membership.len()
    }

    /// Whether this level has an artifact at `ordinal` in the view this form was built for — a
    /// group-scoped layer's artifact belonging to another view occupies no slot here, exactly as a
    /// hole. [`ArtifactView::verdict`](super::ArtifactView::verdict) reads it first.
    pub fn holds(&self, ordinal: u32) -> bool {
        self.membership.holds(ordinal)
    }

    pub fn is_empty(&self) -> bool {
        self.membership.is_empty()
    }

    /// The masked count: how many of this artifact's members this viewer can see. This is the
    /// number served, unmodified, and also the number the existence criterion reads.
    pub fn masked_count(&self, ordinal: u32, mask: &impl MaskedSet) -> u64 {
        self.get(ordinal)
            .map(|rows| mask.count_intersection(rows))
            .unwrap_or(0)
    }

    /// Whether this artifact has any visible member inside `tile_rows` — candidacy, answered as a
    /// masked question.
    pub fn intersects(&self, ordinal: u32, tile_rows: &Bitmap, mask: &impl MaskedSet) -> bool {
        let Some(rows) = self.get(ordinal) else {
            return false;
        };
        // Narrowed to the viewport first: cheap, and it keeps the mask question off every artifact
        // the viewer is not looking at.
        let in_tiles = rows.and(tile_rows);
        !in_tiles.is_empty() && mask.intersects_set(&in_tiles)
    }

    /// The same question against a hoisted `viewport ∩ M_auth`: one early-exiting probe, same
    /// answer as [`Self::intersects`] but paid once for the request instead of once per artifact.
    /// Where the artifact is settled it also answers `masked_count > 0`, the layer-wide question a
    /// criterion reads, since `membership ⊆ viewport` makes the two sets equal. `here` must come
    /// from the composed mask and nothing else — see [`MaskedSet::visible_rows`].
    pub fn intersects_visible(&self, ordinal: u32, here: &Bitmap) -> bool {
        self.get(ordinal).is_some_and(|rows| rows.intersect(here))
    }

    /// Candidacy, on whichever of the three routes the walk's classification makes cheapest —
    /// every one a masked probe, all asking whether `membership ∩ viewport ∩ M_auth` is non-empty.
    /// Settled and open-inside-extent use [`Self::intersects_visible`] against the hoisted
    /// `viewport ∩ M_auth`; everything else, the `everywhere` set included, uses
    /// [`Self::intersects`]. The settled case may not skip the probe on containment alone: an
    /// artifact all of whose members lie outside `M_auth` would then be served, disclosing that a
    /// grouping exists where the viewer can see nothing of it. Nothing here returns a verdict —
    /// [`ArtifactView::verdict`](super::ArtifactView::verdict) still runs for every candidate this admits.
    pub fn candidate_in(
        &self,
        ordinal: u32,
        candidates: &Candidacy,
        viewport: &crate::tile_index::Viewport<'_>,
        mask: &impl MaskedSet,
    ) -> bool {
        let Candidacy::Indexed(candidates) = candidates else {
            return true;
        };
        if candidates.is_settled(ordinal)
            || self
                .index
                .inside(ordinal, viewport.rows(), viewport.cardinality())
        {
            self.intersects_visible(ordinal, viewport.here())
        } else {
            self.intersects(ordinal, viewport.rows(), mask)
        }
    }

    /// The rank of the first content this viewer is served — the containment test. The artifact is
    /// absent, not served without its content, where it carries contents and the viewer satisfies
    /// none. Containment is `|G ∩ M| == |G|`, not a coverage fraction: what decides is which
    /// documents, never how many. Pass and fail cost the same, deliberately: both take one
    /// `count_intersection` over the whole set, with no early exit that would make response time a
    /// function of how close a viewer came.
    pub fn satisfied_rank(
        &self,
        ordinal: u32,
        mask: &impl MaskedSet,
        layer_declares_content: bool,
    ) -> Containment {
        let declared = self.records.declared(ordinal);
        let generating = self.membership.generating(ordinal);
        if declared.is_empty() {
            // An artifact here with none has had its last content withdrawn by a fold, so it is
            // absent until the caller republishes rather than served with the description missing.
            return if layer_declares_content {
                Containment::Unsatisfied
            } else {
                Containment::NothingToContain
            };
        }
        for (i, (rows, declared)) in generating.iter().zip(declared).enumerate() {
            // A set that lost members in projection can never be contained.
            if rows.cardinality() != *declared {
                continue;
            }
            if mask.count_intersection(rows) == *declared {
                return Containment::Satisfied(i as u32);
            }
        }
        Containment::Unsatisfied
    }

    /// [`Self::satisfied_rank`] answered from the containment partition instead of from the mask —
    /// the fast arm, and `None` where the partition cannot answer and the caller must fall back.
    /// The middle test — does this viewer hold every member — moves from a mask intersection to a
    /// lookup, and the deny correction is asked live against the generation's own deny mask. `None`
    /// is not an answer: a partition that does not cover this ordinal, or whose ranks disagree with
    /// the row form's, sends the caller to the route that asks `M_auth` itself.
    ///
    /// An ordinal whose generating sets reach above the base rows declines here: an entity arriving
    /// by ingest is not part of the prefix's postings the partition is composed from, so its clause
    /// would be empty and unsatisfiable for everyone. The masked-count route answers instead.
    pub fn satisfied_rank_via(
        &self,
        ordinal: u32,
        answers: &ContainmentAnswers<'_>,
        denied: &Bitmap,
        layer_declares_content: bool,
    ) -> Option<Containment> {
        if !answers.covers(ordinal) {
            return None;
        }
        if self.membership.generating_above(ordinal, self.base_rows) {
            return None;
        }
        let declared = self.records.declared(ordinal);
        let generating = self.membership.generating(ordinal);
        if declared.is_empty() {
            return Some(if layer_declares_content {
                Containment::Unsatisfied
            } else {
                Containment::NothingToContain
            });
        }
        for (i, (rows, declared)) in generating.iter().zip(declared).enumerate() {
            if rows.cardinality() != *declared {
                continue;
            }
            if !answers.satisfies(ordinal, i)? {
                continue;
            }
            // The acceptance test: a deletion or a suppression removes a member whatever the
            // viewer's terms say, asked here live against the deny mask.
            if denied.intersect(rows) {
                continue;
            }
            return Some(Containment::Satisfied(i as u32));
        }
        Some(Containment::Unsatisfied)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::artifacts::test_support::*;
    use rustc_hash::FxHashSet;
    use tessera_types::TermId;

    /// Candidacy is a masked question. An artifact whose members are all in the tile but none in
    /// the viewer's mask is not a candidate — which is what the deleted bounding box got wrong.
    #[test]
    fn candidacy_is_masked_and_not_a_box() {
        let fx = Fixture::new(&[&[10, 11, 12]], &[1, 2, 3]);
        let tile = Bitmap::of(&[8, 9, 10, 11, 12, 13]);
        assert!(
            !fx.rows.intersects(0, &tile, &fx.mask),
            "every member is inside the tile and none is visible; a box would have served it"
        );

        let visible = Fixture::new(&[&[10, 11, 12]], &[11]);
        assert!(visible.rows.intersects(0, &tile, &visible.mask));

        // A hole is not a candidate either, and must not panic.
        assert!(!fx.rows.intersects(7, &tile, &fx.mask));
    }

    // ---- the containment partition's arm -----------------------------------------------------

    /// A partition built from clauses a test names directly, for cases about the predicate's arm
    /// rather than the signature inversion.
    fn partition_of(clauses_per_rank: &[&[&[u32]]]) -> ContainmentPartition {
        ContainmentPartition::of_clauses(&[clauses_per_rank])
    }

    fn satisfied_terms(terms: &[u32]) -> FxHashSet<TermId> {
        terms.iter().map(|t| TermId::new(*t)).collect()
    }

    /// The two arms return the same rank for a viewer failing the full sample and satisfying the
    /// narrow one.
    #[test]
    fn the_partition_and_the_mask_agree_on_the_served_rank() {
        // Rank 0 is generated from rows 1..=4, terms 7 and 8; rank 1 from rows 1..=2, term 7 alone.
        let rows = rows_with_contents(&[1, 2, 3, 4], &[(&[1, 2, 3, 4], 4), (&[1, 2], 2)])
            .with_partition(Some(partition_of(&[&[&[7], &[8]], &[&[7]]])));
        let partition = rows.partition().expect("the fixture attached one");

        for (held, visible, expected) in [
            (&[7u32, 8u32][..], &[1u32, 2, 3, 4][..], Some(0u32)),
            (&[7], &[1, 2], Some(1)),
            (&[8], &[3, 4], None),
        ] {
            let mask = Bitmap::of(visible);
            let held = satisfied_terms(held);
            let answers = partition.answers(&held);
            let expected = match expected {
                Some(rank) => Containment::Satisfied(rank),
                None => Containment::Unsatisfied,
            };
            assert_eq!(
                rows.satisfied_rank(0, &mask, true),
                expected,
                "the masked-count route disagrees with the case's own arithmetic"
            );
            assert_eq!(
                rows.satisfied_rank_via(0, &answers, &Bitmap::new(), true),
                Some(expected),
                "the partition's arm disagrees with the masked-count route"
            );
        }
    }

    /// One member of rank 0's generating set is suppressed, so the artifact is served rank 1
    /// instead. Without the correction the partition would serve content generated from a document
    /// the viewer may no longer see.
    #[test]
    fn a_denied_member_of_the_generating_set_fails_containment_through_the_partition() {
        let rows = rows_with_contents(&[1, 2, 3, 4], &[(&[1, 2, 3, 4], 4), (&[1, 2], 2)])
            .with_partition(Some(partition_of(&[&[&[7]], &[&[7]]])));
        let partition = rows.partition().unwrap();
        let held = satisfied_terms(&[7]);
        let answers = partition.answers(&held);

        assert_eq!(
            rows.satisfied_rank_via(0, &answers, &Bitmap::new(), true),
            Some(Containment::Satisfied(0))
        );

        // Row 4 suppressed — a member of rank 0's set and of nothing else.
        assert_eq!(
            rows.satisfied_rank_via(0, &answers, &Bitmap::of(&[4]), true),
            Some(Containment::Satisfied(1)),
            "the expression still holds and the member is gone, so the next rank answers"
        );
        assert_eq!(
            rows.satisfied_rank(0, &Bitmap::of(&[1, 2, 3]), true),
            Containment::Satisfied(1),
            "and the masked-count route says the same, which is what makes it a correction \
             rather than a second rule"
        );

        assert_eq!(
            rows.satisfied_rank_via(0, &answers, &Bitmap::of(&[1, 4]), true),
            Some(Containment::Unsatisfied)
        );
        assert_eq!(
            rows.satisfied_rank(0, &Bitmap::of(&[2, 3]), true),
            Containment::Unsatisfied
        );
    }

    /// A generating set that lost a member on the way into row space can never be contained,
    /// however completely the viewer's terms cover what survived.
    #[test]
    fn the_partition_still_refuses_a_set_that_did_not_survive_projection() {
        let rows = rows_with_contents(&[1, 2, 3], &[(&[1, 2], 4)])
            .with_partition(Some(partition_of(&[&[&[7]]])));
        let partition = rows.partition().unwrap();
        let held = satisfied_terms(&[7]);
        let answers = partition.answers(&held);
        assert_eq!(
            rows.satisfied_rank_via(0, &answers, &Bitmap::new(), true),
            Some(Containment::Unsatisfied),
            "a viewer who can see every row there is must still not be served a set that lost \
             members on the way into row space"
        );
    }

    /// A partition that does not cover the ordinal declines rather than answering. `None` is not a
    /// verdict: collapsing it to `Unsatisfied` would withhold every artifact past a short partition.
    #[test]
    fn a_partition_that_does_not_cover_the_ordinal_declines() {
        let rows = rows_of(&[&[1, 2], &[3, 4]])
            .with_partition(Some(ContainmentPartition::of_clauses(&[&[]])));
        let partition = rows.partition().unwrap();
        let held = satisfied_terms(&[7]);
        let answers = partition.answers(&held);
        assert!(answers.covers(0));
        assert_eq!(
            rows.satisfied_rank_via(1, &answers, &Bitmap::new(), true),
            None,
            "the second ordinal is past the partition, so it has no answer to give"
        );
    }

    // ---- candidacy on a row-major level ---------------------------------------------------

    /// A row-major level over twelve artifacts of fifty contiguous rows each, with the column the
    /// scan and the histogram are both read from.
    fn row_major_level() -> (ArtifactRows, Arc<RowColumn>) {
        static DIR: std::sync::OnceLock<tempfile::TempDir> = std::sync::OnceLock::new();
        let scratch = DIR
            .get_or_init(|| tempfile::tempdir().expect("a scratch directory"))
            .path();
        let sets: Vec<Vec<u32>> = (0..12u32)
            .map(|i| ((i * 50)..(i * 50 + 50)).collect())
            .collect();
        let slices: Vec<Option<&[u32]>> = sets.iter().map(|s| Some(s.as_slice())).collect();
        let membership = MembershipRows::of_rows(
            sets.iter()
                .map(|s| Some(s.iter().copied().collect::<Bitmap>()))
                .collect(),
        );
        let column = Arc::new(
            RowColumn::compose(&membership, 600, ServingLayout::RowMajorLabel, scratch)
                .expect("the memberships partition"),
        );
        (
            ArtifactRows::synthetic(&slices, Some(Arc::clone(&column))),
            column,
        )
    }

    /// The histogram and the scan return the same candidates, whatever the viewport covers. A
    /// narrower-than-mask viewport falls back to the scan even with counts in hand, since the
    /// histogram knows nothing about the box.
    #[test]
    fn the_whole_map_reads_candidacy_off_the_histogram_and_a_narrower_viewport_scans() {
        let (rows, column) = row_major_level();
        // Empties ordinal 4 entirely, so it is in the comparison.
        let mut mask: Bitmap = (0..600u32).filter(|r| r % 4 != 3).collect();
        mask.remove_range(200..250);
        let counts = crate::histogram::MaskedCounts::new(column.histogram_over(&mask));
        assert_eq!(counts.get(4), 0, "ordinal 4 has no visible row");

        let whole = Bitmap::from_range(0..600);
        let viewport = crate::tile_index::Viewport::compose(&whole, &mask);
        assert!(viewport.covers_mask());
        let from_histogram: Vec<u32> = rows.candidacy(&viewport, Some(&counts)).iter().collect();
        let from_scan: Vec<u32> = rows.candidacy(&viewport, None).iter().collect();
        assert_eq!(from_histogram, from_scan);
        assert_eq!(from_scan, vec![0, 1, 2, 3, 5, 6, 7, 8, 9, 10, 11]);

        let box_rows = Bitmap::from_range(320..440);
        let narrow = crate::tile_index::Viewport::compose(&box_rows, &mask);
        assert!(!narrow.covers_mask());
        let narrowed: Vec<u32> = rows.candidacy(&narrow, Some(&counts)).iter().collect();
        assert_eq!(
            narrowed,
            rows.candidacy(&narrow, None).iter().collect::<Vec<_>>()
        );
        assert_eq!(narrowed, vec![6, 7, 8]);
    }

    /// A mask holding one row outside the viewport takes the scan, not the histogram: reading the
    /// histogram there would serve an artifact whose only visible member is off screen.
    #[test]
    fn a_viewport_missing_one_visible_row_does_not_cover_the_mask() {
        let (rows, column) = row_major_level();
        let mask: Bitmap = (0..600u32).collect();
        let counts = crate::histogram::MaskedCounts::new(column.histogram_over(&mask));

        // Every row but one, and the one left out is the only visible row of ordinal 11 in view.
        let mut almost = Bitmap::from_range(0..600);
        almost.remove_range(551..600);
        let viewport = crate::tile_index::Viewport::compose(&almost, &mask);
        assert!(!viewport.covers_mask());
        let served: Vec<u32> = rows.candidacy(&viewport, Some(&counts)).iter().collect();
        assert_eq!(
            served,
            (0..12u32).collect::<Vec<_>>(),
            "row 550 is in view and ordinal 11 labels it"
        );

        let mut off_screen = Bitmap::from_range(0..600);
        off_screen.remove_range(550..600);
        let viewport = crate::tile_index::Viewport::compose(&off_screen, &mask);
        assert!(!viewport.covers_mask());
        assert_eq!(
            rows.candidacy(&viewport, Some(&counts))
                .iter()
                .collect::<Vec<_>>(),
            (0..11u32).collect::<Vec<_>>(),
            "ordinal 11 is wholly off screen, and the histogram must not serve it"
        );
    }
}
