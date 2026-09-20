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

/// One level's per-ordinal facts that **no row space is involved in**: the attachment edge, the
/// parent edge, and each content's declared generating-set size.
///
/// **Split out of [`ArtifactRows`] because the two halves cost different amounts and are wanted at
/// different cadences.** Building this is a walk over the level's records copying three small
/// things per artifact; building [`MembershipRows`] beside it decodes and projects every
/// membership, which is the 376 s at 10⁷ artifacts that
/// `design/artifact-serving-at-scale.md` §8.1 measures. Everything `verdict` asks that is not a
/// masked question is answered from here, so a later stage may rebuild one half without the other.
/// Today both are built together under one [`ProjectionKey`](super::ProjectionKey), which is what keeps them describing
/// the same population — see [`ArtifactRows`]' one-snapshot note.
///
/// **What it deliberately does not hold.** The generating sets themselves stay in the registry:
/// the containment partition composes from them inside the same store borrow that builds this, and
/// a second entity-space copy of `Σ|G|` bitmaps would be residency spent to avoid a walk that is
/// already paid. And the proportional criterion's denominator is **not** here, because
/// [`ArtifactView::declared_size`](super::view::ArtifactView::declared_size) takes it from the row form on purpose — numerator and
/// denominator from one projection. An entity-space copy beside it would be a second, larger
/// number with the same name, and §10 of the scale design puts the per-artifact declared size with
/// the row-major layout that needs it rather than here.
#[derive(Debug, Clone, Default)]
pub struct ArtifactRecords {
    /// Per ordinal, what this artifact is an attachment to — `None` for an ordinary artifact.
    ///
    /// **Entity space, and deliberately not projected.** A target is tested on its disposition and
    /// on its layer's gate, neither of which is a row-space question, so a projection would be a
    /// second address for something already addressed.
    pub(super) attachments: Vec<Option<Attachment>>,
    /// Per ordinal, this artifact's parent edges as the registry holds them — ascending by
    /// `(level, ordinal)`, empty at a root, several on a `dag` layer (`dag-hierarchies.md` §7,
    /// decision 0117). Read by the serving path to name a parent that is *also* in the response,
    /// and by nothing in [`ArtifactView`]. It is not a visibility term: see
    /// [`ArtifactRecord::parents`].
    pub(super) parents: Vec<Vec<ParentRef>>,
    /// Per ordinal, per rank: `|G|` in **entity space**, from the durable record.
    ///
    /// **Kept beside the projected set because a projection that lost a member must not read as
    /// containment.** A generating set is entity-space and permanent; row space holds only what
    /// this view has folded in, so a member awaiting a fold projects to nothing and would silently
    /// drop out of the test — leaving a viewer contained in a *smaller* set than the caller
    /// declared, which is the whole disclosure. Carrying the declared size makes the loss
    /// detectable, and a lossy projection fails containment for everybody rather than passing it
    /// for somebody.
    ///
    /// **Rank here is the position in the artifact's ranked `contents`** — not a Morton rank and
    /// not a rank within a bitmap.
    pub(super) declared: Vec<Vec<u64>>,
}

/// One layer's membership in the row space of one view, built at open and rebuilt when the
/// generation moves — the expensive half of [`ArtifactRows`].
///
/// **Built member-wise, and this is a disclosure rule.** Projecting an entity *range* to a row
/// range would admit whatever documents happen to sit between two members in Morton order — and one
/// extra member can lift an artifact over its existence criterion. The write cycle forbids
/// range-wise translation for that reason; the same rule reaches the build of this form.
#[derive(Debug, Clone)]
pub struct MembershipRows {
    /// Parallel to a level's ordinals; `None` is a hole, not an empty membership.
    ///
    /// **One `Arc` per artifact, so cloning the level is the ordinals and not the members.** A
    /// write amends the form in place, and the form is shared with whichever requests are reading
    /// it — so `Arc::make_mut` on the family copies it, and at rung 3's `mesh/descriptors` a copy
    /// of 1.66×10⁹ entries was a *measured* 2 576 ms of the 2 581 ms the whole amendment took, on
    /// the executor thread. Behind an `Arc` each the copy is 30,217 pointers and the one artifact
    /// that grew is the one bitmap made mutable.
    pub(super) rows: Vec<Option<Arc<Bitmap>>>,
    /// Per ordinal, per rank: that content's **generating set** in row space. Pushed in lockstep
    /// with [`ArtifactRecords::declared`], which is the size the same set had in entity space.
    pub(super) generating: Vec<Vec<Bitmap>>,
    /// **Whether [`Self::rows`] holds anything at all.**
    ///
    /// `false` on a level served row-major from a column the prefix holds: the column is the
    /// membership addressed by row, each artifact's extent is folded out of the column's own bytes
    /// ([`RowColumn::extents`]), and nothing is transposed back into an artifact-major bitmap per
    /// ordinal. At the rung 6 corpus — 1,646,192 artifacts over ~3.4×10⁹ member entries a level —
    /// that form is a measured ~28 GB retained and ~10 GB transient, which is the residency
    /// `design/artifact-serving-at-scale.md` §5.1 records and this does not pay.
    ///
    /// **Every write reaches the column, and the form is never transposed back.** A flush, a
    /// growth, a publication and a merge's rebase hand the column the `(row, ordinal)` pairs they
    /// added and widen the extents by the rows those pairs name ([`TileIndex::amend`]); a deny
    /// moves nothing here, being asked of the overlay at every verdict. An amendment a **label**
    /// column cannot express — a row that would come to carry two artifacts — takes the **list**
    /// form instead ([`RowColumn::recompose_as_list`]), composed from the column it already holds
    /// through the disk-backed partition route, so nothing row-sized is held there either.
    ///
    /// The slots are still there and still tell a hole from a live ordinal — every live one holds
    /// the same empty bitmap — which is what [`RowColumn::extents`] needs and what a publication
    /// widens.
    ///
    /// **[`Self::get`] answers `None` for every ordinal while this is `false`**, so a reader that
    /// wants one artifact's rows is told they are not held rather than handed an empty set. What
    /// each such reader takes instead is named at [`ArtifactRows::visible_rows`].
    pub(super) rows_held: bool,
}

/// One level's row form and the records beside it, under one validity key.
///
/// **One snapshot, and it is the whole family rather than two halves.** The records, the
/// projection, the tile index over it and the containment partition are all built from a single
/// borrow of the [`ArtifactStore`] at a single level version, so a write landing between two reads
/// cannot leave one of them describing a population the others no longer have —
/// `2026-08-21-artifact-layout-selection.md` §9's first constraint. The failure it names is
/// specific: a membership that **grew** between two reads leaves the extent beside it narrow, and a
/// narrow extent settles an artifact whose members reach outside the viewport, which is exactly the
/// case the settled half's collapse is not true for.
#[derive(Debug, Clone, Default)]
pub struct ArtifactRows {
    pub(super) records: ArtifactRecords,
    pub(super) membership: MembershipRows,
    /// The hierarchical row-range index and the per-artifact extents — always present, because the
    /// walk is how candidacy is answered rather than an optimisation over answering it another
    /// way. A level with no artifacts has an empty one.
    pub(super) index: TileIndex,
    /// The containment partition, where this level has one.
    ///
    /// `None` under any plugin but the builtin — see [`crate::containment`], whose gate is settled
    /// fail-closed — and containment then stays on the masked-count route, which asks `M_auth`
    /// itself and so cannot depend on the shape of the rule that produced it.
    pub(super) partition: Option<ContainmentPartition>,
    /// **Which form this level is served in**, and the row-addressed column where that form has one
    /// (decision 0094).
    ///
    /// The two travel together and are set together, because the second is what makes the first
    /// true: a level *recorded* row-major whose column would not compose — its memberships turned
    /// out to overlap, or its file would not open — is **served** artifact-major, and this field
    /// says so. Nothing downstream ever has to ask whether the column matching the layout is
    /// present.
    pub(super) layout: ServingLayout,
    pub(super) column: Option<Arc<RowColumn>>,
    /// **Which row space this form's memberships were projected through** — the base row count, and
    /// the `seg_id` of every extent whose rows are in them, in order.
    ///
    /// The two exist because the form now covers extent rows (see [`MembershipRows::put`]) and a
    /// flush brings it forward in place rather than rebuilding it ([`Self::extend_by`]). Extent
    /// rows are the rows a **merge** renumbers, so a form that held them and could not say which
    /// segments they came from would serve one segment's rows as another's after a merge — a
    /// masked count over other people's documents, which is the one direction this may never fail
    /// in.
    ///
    /// **The whole list rather than a count and a boundary id**, which is what
    /// [`crate::projection::RowProjection`] carries. That one is a session's and is only ever
    /// extended; this one is shared, and is asked about by requests at **two** live generations at
    /// once — a session may be served one geometry behind the newest (decision 0044). A form that
    /// had to be at exactly the asker's extent count would then be rebuilt by each of the two in
    /// turn, for ever, which is a thrash and not a wrong answer. Comparing the shared prefix
    /// answers both directions and is exact for `RowProjection`'s own reason: `seg_id`s are never
    /// reused (contracts §2.1).
    pub(super) base_rows: u32,
    pub(super) covered: Vec<String>,
    /// What this form borrowed and the version it borrowed it at: one entry per distinct
    /// `(layer, level)` an artifact of this level took its membership from, because it declared
    /// none of its own ([`ArtifactRows::inherit`]). Empty on a level that borrows nothing, which is
    /// every level carrying no attachment.
    ///
    /// The projection key cannot carry this staleness term. A form is filed under its own level's
    /// version, and a target that grows moves the target's version and not this level's, so a label
    /// of a cluster that gained members would go on answering over the membership the cluster had
    /// when the label's form was built. Read at every cache hit
    /// ([`ArtifactRows::inherited_current`]) beside [`ArtifactRows::covers`]. A form whose
    /// borrowing has moved is rebuilt rather than brought forward: the delta a publication or a
    /// growth applies is the target level's, and this level has no delta to take it.
    pub(super) inherited: Vec<(String, u32, u64)>,
}

/// What one viewport's narrowing produced, on whichever route the level's layout takes.
///
/// **Neither variant is a verdict**, and that is the property both halves share:
/// [`ArtifactView::verdict`](super::ArtifactView::verdict) runs for every ordinal either of them hands back.
pub enum Candidacy {
    /// The artifact-major route: the tile index's walk, with the settled half carried so a probe can
    /// be exact for the mask-shaped question too.
    Indexed(crate::tile_index::Candidates),
    /// The row-major route: one scan of `viewport ∩ M_auth` marking labels.
    ///
    /// **Every ordinal here has already paid its masked probe**, because the scan was over the
    /// visible rows: a row in `viewport ∩ M_auth` is visible by construction, so the artifact
    /// labelling it has a visible member in view. That is the same question the artifact-major
    /// route reaches through the walk and a per-candidate probe, answered once for the whole level.
    Scanned(Bitmap),
}

/// What a filtered request's hoisted `viewport ∩ M_auth ∩ M_sel` produced, on whichever route the
/// level's layout takes — see [`ArtifactRows::matched`].
///
/// **Neither variant admits or withholds an artifact.** The bit rides beside a served artifact and
/// moves nothing else: existence and the masked count are anchored on `M_auth`, filter or no filter
/// (**I3**, **I12**).
pub enum Matched<'a> {
    /// The answer for the whole level, one pass over the matched set — the row-major route and a
    /// spatial level's ranges, exactly as [`Candidacy::Scanned`] is reached.
    Scanned(Bitmap),
    /// The matched set itself, borrowed, for the artifact-major route: one early-exiting probe per
    /// served artifact against it, and nothing materialised per level.
    PerArtifact(&'a Bitmap),
}

impl Candidacy {
    /// Every candidate ordinal, **ascending** — the order the cut downstream is entitled to, and
    /// which both routes produce because both are backed by a bitmap.
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

/// The containment test's three outcomes.
///
/// **`NothingToContain` and `Unsatisfied` are not the same answer**, which is the whole reason this
/// is an enum and not an `Option`: the first is a layer that declares no supplied content, whose
/// artifacts serve on their other conjuncts; the second is an artifact that has a description this
/// viewer may not read, and is therefore **absent**. Collapsing them serves the second case with its
/// content missing — the in-between state decision 0076 forbids.
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
    /// Walk a level's records for the three per-ordinal facts that need no row space.
    ///
    /// Cheap — no membership is decoded and nothing is projected — which is the whole reason this
    /// is separable from [`MembershipRows::build`].
    pub fn build<'a>(artifacts: impl Iterator<Item = (u32, &'a ArtifactRecord)>) -> Self {
        let mut records = ArtifactRecords::default();
        for (ordinal, record) in artifacts {
            records.put(ordinal as usize, record);
        }
        records
    }

    /// Place one record at `idx`, growing the dense vectors to reach it. A slot never written is a
    /// **hole**, not an empty artifact — see [`ArtifactStore`].
    pub(super) fn put(&mut self, idx: usize, record: &ArtifactRecord) {
        if self.attachments.len() <= idx {
            self.attachments.resize_with(idx + 1, || None);
            self.parents.resize_with(idx + 1, Vec::new);
            self.declared.resize_with(idx + 1, Vec::new);
        }
        self.attachments[idx] = record.attached_to.clone();
        self.parents[idx] = record.parents.clone();
        // **The set's stored cardinality, moved by the page that joined or left it** (`ingest.md`
        // §1.1), rather than a count taken of the set here. The two agree — a page carries the
        // number the delta produces and the store refuses one that disagrees — and reading the
        // stored one is what makes the pair this form publishes the pair the page moved.
        self.declared[idx] = record.contents.iter().map(|v| v.cardinality).collect();
    }

    /// What the artifact at `ordinal` hangs from, if it hangs from anything.
    pub(crate) fn attachment(&self, ordinal: u32) -> Option<&Attachment> {
        self.attachments
            .get(ordinal as usize)
            .and_then(Option::as_ref)
    }

    /// The artifact's parent edges, as the registry holds them — ascending by ordinal, empty at
    /// a root and at a hole. A tree's list is at most one long; a `dag` layer's may name several
    /// (`dag-hierarchies.md` §4). **The only read the engine makes of the record's parents**, so
    /// the serving path and the lineage see one shape whatever the record stores.
    pub(crate) fn parents(&self, ordinal: u32) -> &[ParentRef] {
        self.parents
            .get(ordinal as usize)
            .map_or(&[], Vec::as_slice)
    }

    /// **Public for the differential**, on [`MembershipRows::generating`]'s reason: the pair a
    /// containment test reads is the operator and this number, and a test that compared only the
    /// operator would pass a form whose cardinalities came from another version of the set.
    pub fn declared_sizes(&self, ordinal: u32) -> &[u64] {
        self.declared(ordinal)
    }

    /// `|G|` per rank, entity space. Empty for a hole and for an artifact with no contents alike —
    /// the two are told apart by [`ArtifactRows::satisfied_rank`] against the layer's declaration,
    /// never here.
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
    /// Project a level's memberships into `space`.
    ///
    /// Costly by design and not on any per-request path: `RowSpace::project` decodes the whole
    /// membership. It is paid at open and at a generation move, which is the same cadence the
    /// session's own mask projection is paid at.
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
        // **The whole row space — base and every extent** — and the extents are what a flush then
        // adds to in place ([`ArtifactRows::extend_by`]) rather than what a rebuild recovers. The
        // form was base-only until 2026-09-03, which made a member ingested since the last fold
        // contribute nothing to its artifact's count until that fold: fail-closed, and hours wide
        // under the nightly gate. What made base-only necessary was that a form covering extents
        // had no way to *stay* covering them; it has one now, and the merge that renumbers extent
        // rows is caught by [`ArtifactRows::covers`] before a stale form is ever served.
        let rows = Arc::new(space.project(&record.members));
        // **A column-only form keeps the slot and not the set**: the slot is what tells a hole from
        // a live artifact, and the column is where the rows are. The `Arc` is shared rather than
        // the bitmap copied, so the projecting build pays nothing for handing the rows back.
        self.rows[idx] = Some(if self.rows_held {
            Arc::clone(&rows)
        } else {
            Arc::new(Bitmap::new())
        });
        self.generating[idx] = record
            .contents
            .iter()
            // **The whole row space, as the membership above is.** A member's row is a member's
            // row wherever it lies, so a member that arrived by ingest counts from the flush that
            // gives it one. A member still in the commit buffer has no row, the set projects short
            // of its declared size, and [`ArtifactRows::satisfied_rank`] withholds the content
            // from everyone until the flush — the direction this must fail in.
            //
            // The containment partition is composed from the build's postings, which describe base
            // rows alone, so it declines an ordinal whose set reaches above them
            // ([`ArtifactRows::satisfied_rank_via`]) and the exact masked-count route answers.
            .map(|v| space.project(&v.generated_from))
            .collect();
        rows
    }

    /// One artifact's **generating sets alone**, with an empty membership standing in for rows a
    /// transposed column supplies afterwards ([`Self::absorb_transposed`]).
    ///
    /// **The generating sets still project, and they must.** A row column holds membership and
    /// nothing else: containment is tested against each content's generating set in row space,
    /// which is a different set from the membership and is nowhere in the column. It is also
    /// small — `Σ|G|` over a level's contents, a sample per content rather than a corpus —  so
    /// projecting it costs a fraction of the membership this route is avoiding.
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

    /// One artifact's generating sets projected again from the record — the tick's whole arm,
    /// where a page held a leave or a fill changed which contents the artifact has. The
    /// membership is left where it is: neither route touches it.
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
    /// empty — every live ordinal, and no hole.
    ///
    /// `false` where `transposed` does not reach every ordinal this level has records for, which
    /// is a column narrower than the level it was adopted for: the caller projects instead. A
    /// slot no record occupies stays `None` — a hole is not an artifact with no members, and the
    /// column cannot tell the two apart because a hole and an empty membership label the same
    /// rows, which is none.
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
    /// [`Self::rows_held`]. Every live ordinal keeps the one shared empty bitmap, so a hole is
    /// still a hole and [`Self::absorb_transposed`] can bring the form back.
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
    /// the rows are not this artifact's own and it has nothing of its own to keep. A membership is
    /// never otherwise replaced: it grows ([`Self::or_rows`]) or it is rebased over one extent
    /// ([`Self::rebase_rows`]).
    pub(super) fn put_rows(&mut self, idx: usize, rows: Bitmap) {
        if self.rows.len() <= idx {
            self.rows.resize_with(idx + 1, || None);
            self.generating.resize_with(idx + 1, Vec::new);
        }
        self.rows[idx] = Some(Arc::new(rows));
        self.rows_held = true;
    }

    /// Union `rows` into the slot at `idx` — **the only way a held form's membership grows**.
    ///
    /// A slot holding `None` is a **hole** and stays one: an ordinal no record occupies is an
    /// artifact a fold retired, and giving it rows here would resurrect it under an identity a
    /// caller's `tessera_id` still names — [`tessera_lifecycle::membership::ArtifactStore::grow`]'s
    /// rule, one structure along. `false` says nothing was done.
    pub(super) fn or_rows(&mut self, idx: usize, rows: &Bitmap) -> bool {
        match self.rows.get_mut(idx) {
            // A column-only form holds the slot and not the set: the caller's `(row, ordinal)`
            // pairs go to the column and the extents instead, and *whether the ordinal is live* is
            // the only thing this answers ([`Self::rows_held`]).
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
    /// changes without growing, and a merge is the one operation that asks for it: the rows inside
    /// the merged span name other entities afterwards, so the bits there are cleared and the same
    /// members' new rows put in. `remove_range` is O(containers in the span). A hole stays a hole,
    /// on [`Self::or_rows`]' rule.
    ///
    /// **An artifact with no row in the span, before or after, is not touched.** The form is still
    /// in the map while this runs, so every bitmap is shared and `make_mut` copies it; at rung 3's
    /// `mesh/descriptors` the copy of every artifact was a *measured* 3.4 s on the executor thread
    /// for a merge that relabelled nothing. `range_cardinality` is O(containers in the span).
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
    /// [`Self::or_rows`], and the only way a held set grows.
    ///
    /// A slot with no sets is a hole or an ordinal the form does not reach, and takes nothing.
    /// A set is held whether or not the memberships are ([`Self::rows_held`]): the column holds
    /// membership and nothing else.
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

    /// Whether any of this ordinal's generating sets holds a row at or above `base_rows`.
    ///
    /// **One `maximum` a rank**, which is the last container's largest value: O(1) a set, and the
    /// question is per ordinal because the containment partition is addressed per ordinal.
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

    /// See [`Self::rows_held`].
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
    /// ([`Self::hold_no_rows`]), which is why it reads the slot rather than [`Self::get`].
    fn holds(&self, ordinal: u32) -> bool {
        self.rows.get(ordinal as usize).is_some_and(Option::is_some)
    }

    /// A row form given directly — the tests whose subject is the hierarchy over a row form rather
    /// than the projection into one. Not a route a stored membership takes: that goes through
    /// [`Self::put`] and the permutation, and the fold composes a spatial level's column from its
    /// resolved rows without a row form at all.
    #[cfg(test)]
    pub(crate) fn of_rows(rows: Vec<Option<Bitmap>>) -> Self {
        MembershipRows {
            generating: vec![Vec::new(); rows.len()],
            rows: rows.into_iter().map(|set| set.map(Arc::new)).collect(),
            rows_held: true,
        }
    }

    /// The projected generating sets, per rank. Parallel to [`ArtifactRecords::declared`].
    ///
    /// **Public for the differential**, which is the one caller outside this module: a row form
    /// transposed out of a column takes these from the same projection the artifact-major route
    /// takes them from, and `tests/artifact_tile_index.rs` asserts that set by set rather than on
    /// the argument that both call the same line.
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

    /// **Blocks per artifact** — decision 0092's (c), observed over the form rather than declared,
    /// and the number the automatic layout pick's threshold is expressed in.
    ///
    /// `blocks` is Roaring **containers touched**, which is the measured cost model — bitmap
    /// operations cost O(containers touched) rather than O(cardinality) — and so the number that
    /// says whether a level has row-space locality at all: 1.0 for a clustering, 96.8 for a
    /// scattered predicate (`design/artifact-serving-at-scale.md` §5).
    ///
    /// Holes and artifacts whose membership projects to nothing are not counted: a retired slot is
    /// not an artifact, and counting it would keep an emptied level looking populous and make its
    /// mean locality look better than it is.
    ///
    /// **Separate from [`Self::shape`], which is the same walk plus the disjointness observation.**
    /// This one is reported at every form build, so it may not allocate a second copy of the level;
    /// that one runs once per fold, where it can.
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

    /// **The shape the automatic layout pick reads** — [`Self::blocks_per_artifact`] with the
    /// artifact count, the `everywhere` fraction and the disjointness observation beside it.
    ///
    /// **Called once per level per fold**, which is what makes the running union affordable:
    /// whether the memberships are disjoint decides the label/list split and is a property of the
    /// data rather than of the declaration, so there is no cheaper way to learn it than to look.
    ///
    /// `row_count` is the view's base row space, which the `everywhere` test needs: the node ladder
    /// is derived from it ([`tessera_store::derived::observe_shape`]).
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
/// group-scoped layer names (`views.md` §3.5): a group's several layouts over one key set draw
/// the same artifact in each.
pub(crate) fn view_key(view: &str) -> &str {
    tessera_store::view_path_components(view)
        .last()
        .copied()
        .unwrap_or(view)
}

/// **One artifact's record, and only where this view draws it** — the single read every
/// in-place amendment makes of the store, so that a held form takes a delta for its own view's
/// artifacts alone (`views.md` §3.5).
///
/// `view` is the view's path, as the projections are keyed; what a record names is the view's own
/// key ([`view_key`]). `None` for a hole, and for an artifact belonging to another view of the
/// same group: putting such a record into this form would serve that view's key, its
/// `tessera_id` and a live count to a principal of this one, and would label this view's rows
/// with an ordinal it does not draw.
///
/// The projecting routes reach the same rule through
/// [`tessera_lifecycle::membership::ArtifactStore::level_in_view`], which is this test over a
/// whole level.
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
    /// Which form this level is **served** in — see [`ArtifactRows::with_column`] on why that is not
    /// always the form the manifest records.
    pub fn layout(&self) -> ServingLayout {
        self.layout
    }

    /// This level's row-addressed column, where it has one.
    pub fn column(&self) -> Option<&RowColumn> {
        self.column.as_deref()
    }

    /// **Candidacy for one viewport, on whichever route this level's layout takes.**
    ///
    /// The two answer the same question — *which artifacts could have a member this viewer can see
    /// inside the viewport* — and the layout decides only which structure is walked to reach it.
    /// `tests/artifact_row_major.rs` asserts the two agree ordinal for ordinal over a generated
    /// corpus, which is this stage's spine.
    ///
    /// # The whole-map case is read off the histogram rather than scanned
    ///
    /// `counts` is this level's masked-count histogram where the request already built one
    /// ([`crate::Engine::masked_counts`]). It says, per ordinal, how many rows inside `M_auth`
    /// carry that ordinal's label. Where the viewport covers the whole mask
    /// ([`crate::tile_index::Viewport::covers_mask`]) that is the question candidacy asks: `here`
    /// is then `M_auth` itself, so an ordinal has a visible member in view exactly when its count
    /// is non-zero, and the scan finds nothing the histogram has not already counted.
    ///
    /// The two are taken over the same set, and that is what makes the substitution exact. The
    /// histogram walks [`crate::compose::WholeMask::visible_all`] and `here` is composed from the
    /// same three terms, both blind to the request's filter, so the whole-map answer here is the
    /// authorised one whether or not the request carries a filter (I3, I12). The filtered question
    /// is [`Self::matched`]'s, asked of a narrower set, and it keeps the scan.
    ///
    /// What this removes is the scan: 3.5×10⁹ labels read at the rung 6 corpus to learn what the
    /// histogram beside it had already counted.
    pub fn candidacy(
        &self,
        viewport: &crate::tile_index::Viewport<'_>,
        counts: Option<&crate::histogram::MaskedCounts>,
    ) -> Candidacy {
        // **Two routes and one question.** The column arm answers against `viewport ∩ M_auth` and
        // is therefore exact for the masked question as well; the indexed arm is a candidate
        // generator and every ordinal it returns still pays a probe.
        match &self.column {
            Some(column) => {
                if viewport.covers_mask() {
                    // The lengths must agree, or the histogram is not this column's. Both are the
                    // level's ordinal count, and the key the histogram is filed under carries the
                    // version of the form this column came from
                    // (`ArtifactProjections::get_or_build`), so they agree on every route that
                    // reaches here.
                    //
                    // **This is a sanity check on the pairing, not the disclosure defence.** What
                    // keeps an ordinal with no visible row out of the answer is that the counts
                    // were taken over `M_auth` and over nothing else; a length mismatch would only
                    // make the entry short, and a short entry loses artifacts rather than
                    // admitting them. The version term of the key is what stops a histogram of
                    // another version being read here at all.
                    if let Some(counts) = counts.filter(|c| c.len() == column.len()) {
                        return Candidacy::Scanned(counts.populated());
                    }
                }
                Candidacy::Scanned(column.candidates(viewport.here()))
            }
            None => Candidacy::Indexed(self.index.candidates(viewport.rows())),
        }
    }

    /// **Which of this level's artifacts hold a member the request's filter admits**, given the
    /// hoisted `viewport ∩ M_auth ∩ M_sel` — [decision 0104](../../../docs/decisions/0104-a-filter-answers-a-boolean-per-served-artifact.md)'s
    /// bit, in whichever shape the level's layout makes cheapest.
    ///
    /// **The same two routes as [`Self::candidacy`], asked of a narrower set**, and deliberately
    /// so: the question is candidacy's own — *has this artifact a visible member in view* — with
    /// the filter's rows removed from the input first. So the row-major arm answers for the whole
    /// level in one pass, as it does there, and the artifact-major arm defers to a probe per
    /// artifact, which the caller pays only for the artifacts it actually serves.
    ///
    /// The set handed in must come from [`crate::compose::EffectiveMask::matched_rows`] and from
    /// nothing else, which is what keeps the answer inside `M_auth`.
    pub fn matched<'a>(&self, here_matched: &'a Bitmap) -> Matched<'a> {
        match &self.column {
            Some(column) => Matched::Scanned(column.candidates(here_matched)),
            None => Matched::PerArtifact(here_matched),
        }
    }

    /// Whether one artifact holds such a member — see [`Self::matched`].
    ///
    /// **The probe is early-exiting** (`Bitmap::intersect` stops at the first container that
    /// meets), so it is cheap where there is a hit and a pass over the artifact's containers where
    /// there is not. Under a selective filter the second is the common case, and a scattered
    /// artifact's membership spans hundreds of blocks — unmeasured, and stated rather than claimed
    /// (`artifact-serving-at-scale.md` §7).
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

    /// This level's tile index — the walk that decides which artifacts a viewport asks about.
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

    /// **`membership ∩ M_auth` for one artifact** — the rows of its membership this viewer may
    /// see, and the one question every per-artifact reader of the membership asks: the input
    /// derived content is a function of (`annotations.md` §4.2), the operand a `member_of` leaf
    /// resolves to, and the rows a region leaf by artifact stands for.
    ///
    /// **Two routes and one answer.** Where the form holds per-artifact rows, it is one
    /// intersection with the mask, O(containers touched). Where it does not — a column-only form
    /// ([`MembershipRows::rows_held`]) — it is a walk of the rows this viewer may see **inside the
    /// artifact's extent**, reading each one's labels off the column.
    ///
    /// ⊘ **The extent bounds that walk only as far as the artifact is clustered.** A *scattered*
    /// artifact's extent is the whole row space, so the walk is `|M_auth|` — measured at 2.85 s on
    /// rung 3's `mesh/descriptors` against 22 ms for the bitmap it replaces. **So nothing that runs
    /// per served artifact may call this**: the two readers that do each name one artifact and pay
    /// it once a request, and the viewport's derived centroid and box come from
    /// [`crate::histogram::MaskedGeometry`], one pass over the mask for the whole level.
    ///
    /// **Composed from inside the mask** (**I2**): the span is handed to
    /// [`MaskedSet::visible_rows`], which is the only route to a visible row set, rather than
    /// materialised and gated afterwards.
    ///
    /// Empty for a hole, for an artifact whose membership projects to nothing, and for a
    /// column-only form with no column — which cannot arise, the column being what makes the form
    /// column-only.
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
        // ⊘ **The span's visible rows are materialised before they are walked**, so a scattered
        // artifact — whose span is the row space — costs a copy of `M_auth` on top of the walk:
        // 78.5 B a Roaring container at the residency campaign's measured constant, ~125 MB at 10⁹
        // visible rows. Walking the mask in place instead would need an iterator over
        // `M_auth ∩ range` on [`MaskedSet`], and that trait's whole point is that
        // [`MaskedSet::visible_rows`] is the **only** route to a visible row set (**I2**) — a
        // second one is a second thing that could come to be called with an uncomposed mask. The
        // copy is what that costs, and it is paid once a request by a leaf that names one artifact.
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

    /// **Whether this level has an artifact at `ordinal` in the view this form was built for.**
    ///
    /// A form is built per `(view, layer, level)` from
    /// [`tessera_lifecycle::membership::ArtifactStore::level_in_view`], so a group-scoped layer's
    /// artifact belonging to another view of the group occupies no slot here, exactly as a hole
    /// and an ordinal past the level's end occupy none. All three are the same answer and
    /// [`ArtifactView::verdict`](super::ArtifactView::verdict) reads it first.
    pub fn holds(&self, ordinal: u32) -> bool {
        self.membership.holds(ordinal)
    }

    pub fn is_empty(&self) -> bool {
        self.membership.is_empty()
    }

    /// The masked count: how many of this artifact's members this viewer can see.
    ///
    /// **This is the number served**, unmodified, and it is also the number the criterion reads.
    /// One quantity, computed once, used for both — because a design where the served count and the
    /// tested count could differ is one where they eventually do.
    pub fn masked_count(&self, ordinal: u32, mask: &impl MaskedSet) -> u64 {
        self.get(ordinal)
            .map(|rows| mask.count_intersection(rows))
            .unwrap_or(0)
    }

    /// Whether this artifact has any visible member inside `tile_rows` — candidacy, answered as a
    /// **masked** question. See [`MaskedSet::intersects_set`] for the bounding box this replaces.
    pub fn intersects(&self, ordinal: u32, tile_rows: &Bitmap, mask: &impl MaskedSet) -> bool {
        let Some(rows) = self.get(ordinal) else {
            return false;
        };
        // Narrowed to the viewport **first**: a tile set is a handful of contiguous runs, so this
        // is the cheap term, and it keeps the mask question — the expensive one — off every
        // artifact the viewer is not looking at.
        let in_tiles = rows.and(tile_rows);
        !in_tiles.is_empty() && mask.intersects_set(&in_tiles)
    }

    /// The same question against a **hoisted** `viewport ∩ M_auth`: one early-exiting
    /// `Bitmap::intersect`, which stops at the first container that meets.
    ///
    /// **The same answer as [`Self::intersects`], reached with the composition paid once for the
    /// request instead of once per artifact** — which is what
    /// `design/artifact-serving-at-scale.md` §7.1 measures as load-bearing rather than an
    /// optimisation: at 10⁷ artifacts the same structures without the hoisting cost 3.3 s at the
    /// whole map against 553 ms.
    ///
    /// **And where the artifact is settled it is exact for a second question.** Where
    /// `membership ⊆ viewport`, `membership ∩ (viewport ∩ M_auth)` and `membership ∩ M_auth` are
    /// the same set, so this probe answers `masked_count > 0` — the layer-wide question a criterion
    /// reads — as well as the request's. That collapse is what containment inside a covered node
    /// buys: not a test skipped, but one probe answering both (§4.1).
    ///
    /// **`here` must come from the composed mask** and from nothing else — see
    /// [`MaskedSet::visible_rows`], which is the only way to obtain one.
    pub fn intersects_visible(&self, ordinal: u32, here: &Bitmap) -> bool {
        self.get(ordinal).is_some_and(|rows| rows.intersect(here))
    }

    /// **Candidacy, on whichever of the three routes the walk's classification makes cheapest** —
    /// and every one of them is a masked probe (`design/artifact-serving-at-scale.md` §4 steps 2
    /// and 3).
    ///
    /// - **settled** — the walk took a node the viewport covers entirely, so
    ///   `membership ⊆ viewport` and [`Self::intersects_visible`] against the hoisted
    ///   `viewport ∩ M_auth` is exact for the mask-shaped question as well as this one.
    /// - **open, and inside its extent** — the same collapse, reached by the alignment-free test
    ///   rather than by the node an artifact straddling a boundary was promoted out of.
    /// - **everything else, the `everywhere` set included** — [`Self::intersects`], which narrows
    ///   to the viewport first and asks the mask second.
    ///
    /// **The three agree, always**, because all three ask whether
    /// `membership ∩ viewport ∩ M_auth` is non-empty; what the geometry chooses is the route, never
    /// the answer (§4.1). `tests/artifact_tile_index.rs` asserts that against the sweep this
    /// replaced, ordinal for ordinal.
    ///
    /// ⊘ **An earlier revision of the design let the settled case skip the probe on containment
    /// alone**, and that was fail-open: an artifact all of whose members lie outside `M_auth` would
    /// have been served, disclosing that a grouping exists where the viewer can see nothing of it
    /// (the review's finding 1). Nothing here returns a verdict — [`ArtifactView::verdict`](super::ArtifactView::verdict) still
    /// runs for every candidate this admits.
    /// **On a row-major level there is a fourth route, and it is no route at all**: the scan was
    /// over `viewport ∩ M_auth`, so an ordinal it returned has a visible member in view by
    /// construction and there is nothing left to ask. That is the same collapse the settled case
    /// rests on, reached for the whole level in one pass rather than per artifact.
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

    /// The rank of the first content this viewer is served — the containment test
    /// (`annotations.md` §4).
    ///
    /// `Ok(None)` where the artifact carries no contents at all, which is every artifact on a
    /// layer declaring no supplied content: there is nothing to contain, and the artifact serves on
    /// its other conjuncts alone. `Err(())` where it carries contents and the viewer satisfies
    /// none — the artifact is then **absent**, not served without its content.
    ///
    /// **Containment is `|G ∩ M| == |G|`, and it is not a coverage fraction.** A viewer seeing 60%
    /// of the corpus fails a 240-document set almost surely; one seeing 0.4% satisfies a
    /// single-term set completely. What decides is *which* documents, never how many.
    ///
    /// **Pass and fail cost the same**, deliberately: both take one `count_intersection` over the
    /// whole set — O(containers touched) — with no early exit on the first missing member. A
    /// short-circuiting subset test returns sooner the *less* of the set a viewer holds, which
    /// makes response time a function of how close they came.
    pub fn satisfied_rank(
        &self,
        ordinal: u32,
        mask: &impl MaskedSet,
        layer_declares_content: bool,
    ) -> Containment {
        let declared = self.records.declared(ordinal);
        let generating = self.membership.generating(ordinal);
        if declared.is_empty() {
            // **Nothing to contain, or nothing left to serve — and the layer's declaration is what
            // tells them apart.** A layer declaring no supplied content has artifacts that serve on
            // their other conjuncts; one that *does* declare it has artifacts that must carry it,
            // and an artifact here with none has had its last content withdrawn by a fold because
            // its generating set lost a deleted member (decision 0135). Serving it would be the
            // identity and the count with the description missing — the in-between state decision
            // 0076 forbids — so it is absent until the caller republishes.
            return if layer_declares_content {
                Containment::Unsatisfied
            } else {
                Containment::NothingToContain
            };
        }
        // Both halves were pushed in lockstep from one record, so the zip is total; a shorter
        // projection could only come from a form assembled by hand, and truncating is the
        // fail-closed reading of that.
        for (i, (rows, declared)) in generating.iter().zip(declared).enumerate() {
            // A set that lost members in projection can never be contained — see
            // [`ArtifactRecords::declared`]. Checked before the mask rather than after, because it
            // is a property of the artifact and not of the viewer, and because it must not be
            // expressible as *contained*.
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
    ///
    /// **The same three tests in the same order, and the first and third are unchanged.**
    /// Projection loss is a per-view property of the artifact and stays on the row form; the
    /// **middle** test — *does this viewer hold every member* — is what moves from a mask
    /// intersection to a lookup; and the deny correction is asked of the mask, live, exactly where
    /// the expression would otherwise have passed. A rank that fails any of the three falls
    /// through to the next one, which is what the masked-count route does with a rank whose count
    /// came back short, for whichever of the three reasons it was.
    ///
    /// **`None` is not an answer**, and the distinction is the whole of the fallback's safety: a
    /// partition that does not cover this ordinal, or whose ranks disagree with the row form's,
    /// sends the caller to the route that asks `M_auth` itself rather than answering from a
    /// structure that does not describe the artifact in front of it.
    ///
    /// **An ordinal whose generating sets reach above the base rows declines here.** The partition
    /// is composed from the prefix's `terms/postings.arrow`, which holds the signatures of the
    /// entities the build read; an entity that arrived by ingest owns an extent row and carries its
    /// terms in a delta tier the composer does not read, so its clause would be empty and the
    /// expression unsatisfiable for everyone. The test is one `maximum` a rank
    /// ([`MembershipRows::generating_above`]), and the masked-count route answers instead.
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
            // **The acceptance test.** The expression says the viewer's terms reach every member;
            // a deletion or a suppression removes one whatever the terms say, and this is where
            // that is asked — live, against the generation's own deny mask.
            //
            // Row space rather than entity space, and exact because `row_of` is injective and
            // `denied` is derived over the whole row space (`crate::compose::denied_rows_of`): a
            // member of a set that cleared the projection check above has a row, so
            // `projected ∩ denied_rows = ∅` iff `G ∩ denied = ∅`. Sets reaching above the base rows
            // never arrive here, having declined at the head of this function.
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

    /// A partition over one artifact's ranked contents, built from the clauses a test names
    /// directly rather than from postings — these cases are about the predicate's arm, and
    /// composing signatures would test the inversion instead.
    fn partition_of(clauses_per_rank: &[&[&[u32]]]) -> ContainmentPartition {
        ContainmentPartition::of_clauses(&[clauses_per_rank])
    }

    fn satisfied_terms(terms: &[u32]) -> FxHashSet<TermId> {
        terms.iter().map(|t| TermId::new(*t)).collect()
    }

    /// **The two arms return the same rank**, on the case the design turns on: a viewer failing
    /// the full sample and satisfying the narrow one. The partition answers from terms, the
    /// masked-count route from `M_auth`, and a disagreement would mean one of them is serving
    /// content on a set the other says the viewer does not hold.
    #[test]
    fn the_partition_and_the_mask_agree_on_the_served_rank() {
        // Rank 0 is generated from rows 1..=4, whose entities carry terms 7 and 8; rank 1 from
        // rows 1..=2, term 7 alone.
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

    /// **The deny correction is the acceptance test, not a refinement.** The expression says the
    /// viewer's terms reach every member of rank 0's generating set — and one of those members is
    /// suppressed, so the artifact is served rank 1 instead. Without the correction the partition
    /// serves content generated from a document the viewer may no longer see, which is the
    /// fail-open the write cycle exists to prevent.
    #[test]
    fn a_denied_member_of_the_generating_set_fails_containment_through_the_partition() {
        let rows = rows_with_contents(&[1, 2, 3, 4], &[(&[1, 2, 3, 4], 4), (&[1, 2], 2)])
            .with_partition(Some(partition_of(&[&[&[7]], &[&[7]]])));
        let partition = rows.partition().unwrap();
        let held = satisfied_terms(&[7]);
        let answers = partition.answers(&held);

        // Nothing denied: the caller's first choice.
        assert_eq!(
            rows.satisfied_rank_via(0, &answers, &Bitmap::new(), true),
            Some(Containment::Satisfied(0))
        );

        // Row 4 suppressed — a member of rank 0's set and of nothing else. The mask the
        // masked-count route counts against already has that row taken out, which is what makes
        // the two arms comparable at all.
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

        // And with the narrow set denied too there is nothing left to serve.
        assert_eq!(
            rows.satisfied_rank_via(0, &answers, &Bitmap::of(&[1, 4]), true),
            Some(Containment::Unsatisfied)
        );
        assert_eq!(
            rows.satisfied_rank(0, &Bitmap::of(&[2, 3]), true),
            Containment::Unsatisfied
        );
    }

    /// **Projection loss is not in the expression and must not be lost with it.** A generating set
    /// that lost a member on the way into row space can never be contained, however completely the
    /// viewer's terms cover what survived — the partition's arm checks it first, exactly as the
    /// masked-count route does.
    #[test]
    fn the_partition_still_refuses_a_set_that_did_not_survive_projection() {
        // Two rows survived; the caller published four.
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

    /// A partition that does not cover the ordinal in front of it declines rather than answering,
    /// and the caller falls back to the route that asks `M_auth` itself. **`None` is not a
    /// verdict** — collapsing it to `Unsatisfied` would withhold every artifact past a partition
    /// that was one ordinal short.
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

    /// The histogram and the scan return the same candidates, whatever the viewport covers.
    ///
    /// The whole-map viewport is the case the histogram answers: `here` is then `M_auth` itself, so
    /// the counts taken over `M_auth` already say which ordinals have a visible member. A narrower
    /// viewport is the case it must not answer, the histogram being over the whole mask and knowing
    /// nothing about the box, and the level falls back to the scan even with counts in hand.
    #[test]
    fn the_whole_map_reads_candidacy_off_the_histogram_and_a_narrower_viewport_scans() {
        let (rows, column) = row_major_level();
        // A mask that keeps three quarters of the row space, emptying ordinal 4 entirely so that
        // an ordinal present in the level and absent from the mask is in the comparison.
        let mut mask: Bitmap = (0..600u32).filter(|r| r % 4 != 3).collect();
        mask.remove_range(200..250);
        let counts = crate::histogram::MaskedCounts::new(column.histogram_over(&mask));
        assert_eq!(counts.get(4), 0, "ordinal 4 has no visible row");

        // The whole map: every visible row is in view, so the two routes must agree.
        let whole = Bitmap::from_range(0..600);
        let viewport = crate::tile_index::Viewport::compose(&whole, &mask);
        assert!(viewport.covers_mask());
        let from_histogram: Vec<u32> = rows.candidacy(&viewport, Some(&counts)).iter().collect();
        let from_scan: Vec<u32> = rows.candidacy(&viewport, None).iter().collect();
        assert_eq!(from_histogram, from_scan);
        assert_eq!(from_scan, vec![0, 1, 2, 3, 5, 6, 7, 8, 9, 10, 11]);

        // A viewport holding two artifacts' rows: narrower than the mask, so the histogram is not
        // an answer and must not be taken for one.
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

    /// A viewport that covers most of the row space but not all of the mask is not the whole-map
    /// case. The test is `|viewport ∩ M_auth| = |M_auth|`, so a mask holding one row outside the
    /// viewport takes the scan. That is the direction that matters: reading the histogram there
    /// would serve an artifact whose only visible member is off screen.
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
