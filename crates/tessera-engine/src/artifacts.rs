//! Whether an artifact is served, and what number sits beside it.
//!
//! **One predicate, evaluated on every route.** The viewport, drill-down, filters, search, edge
//! traversal and metadata all call [`ArtifactView::verdict`] and nothing else. Where a route cannot
//! afford it, the route does not exist — that is what keeps the leak register exhaustive by
//! construction rather than by audit.
//!
//! The order of the conjuncts is not cosmetic:
//!
//! 0. **There is an artifact at that ordinal, in this view.** A level's row form is built for one
//!    view of one layer, so a hole, an ordinal past its end and a group-scoped layer's artifact
//!    belonging to another view of the group are one answer: absent ([`ArtifactRows::holds`],
//!    `views.md` §3.5). The ordinal is an address over the whole layer and the view is part of a
//!    scoped artifact's identity, so this is where the two are reconciled. Without it a view of a
//!    group-scoped layer serves the group's other views' artifacts — their keys, their
//!    identifiers, and a masked count of zero, which a layer declaring no existence criterion has
//!    nothing to withhold.
//! 1. **The overlay, first and unconditional.** A suppression applies to every request the moment
//!    it is accepted, whatever else is true, so an artifact reaches the same `deleted > suppressed`
//!    composition a point does, by the same route.
//! 2. **The layer's gate.** Whether this viewer may know the layer exists at all.
//! 3. **The artifact it depends on, if it depends on one — visible to this viewer, entire.** An
//!    artifact published as an attachment to another — a toponymy label on a cluster — is served
//!    only where the artifact it attaches to is served
//!    ([decision 0089](../../../docs/decisions/0089-a-dependency-edge-carries-deletion-and-visibility.md),
//!    rule 2). Without that term the predicate is per-artifact by construction, so suppressing a
//!    cluster stops the cluster serving while every label naming and describing it goes on serving
//!    to whoever reaches it directly: by search, by a held identifier, by a filter. The model's
//!    conjunctive rule covers edge *traversal* and those routes traverse nothing
//!    (`annotation-representation.md` §4).
//!
//!    **The target's whole predicate, and per artifact rather than per layer.** The term was once
//!    three cheaper ones — the target's disposition, its layer's reachability, and whether its slot
//!    still exists — which left a target withheld by *its own* criterion still nameable by a label
//!    on a layer declaring a weaker one
//!    ([decision 0086](../../../docs/decisions/0086-the-attachment-term-does-not-inherit-the-targets-criterion.md),
//!    superseded on this point by 0089). Rule 2 closes it: the prerequisite is the same `verdict`
//!    call evaluated for the target's layer, so the criterion, the own-terms gate and containment
//!    all count, and the conjunction can only narrow what a principal sees. It costs the target's
//!    masked count per attached artifact per request, which is what 0086 declined to pay and 0089
//!    rules is paid.
//!
//!    Existence is inside that call rather than beside it, and the fold is why it has to be asked
//!    at all: an overlay entry says *deleted*, and the fold that executes the deletion retires the
//!    entry in the same publication that drops the target's slot — so a term resting on the overlay
//!    alone would start serving every label attached to a deleted cluster at the next nightly fold.
//!    A hole answers *not served*, which makes the fold's own hole the durable form of the
//!    withholding rather than a state something has to remember.
//!    What it attaches to is also where its membership comes from, where it declared none
//!    (decision 0145). A label with no member rows is the label of its cluster. It is placed where
//!    the cluster is placed and counted over the cluster's members, so the number below and the
//!    tile above read the target's membership. [`ArtifactRows::inherit`] resolves that once, for
//!    every route.
//! 4. **The artifact's own terms, if its layer declared that its artifacts carry them.**
//! 5. **The existence criterion, if declared** — the masked count against a declared bar.
//!
//! Two of those were once one thing, and separating them is
//! [decision 0079](../../../docs/decisions/0079-the-gate-is-one-flag-not-three-modes.md): the three
//! gate modes it replaced were a two-by-two in three names, and *substitutive* switched the
//! criterion off, so a corpus-derived clustering mis-declared served the existence and count of
//! every cluster down to a single member. Under a flag beside an independent criterion, one schema
//! word can no longer disable a disclosure control.
//!
//! ## The count is masked, and the criterion never touches it
//!
//! `|rows(artifact) ∩ M|` where `M` is the session's **composed** mask — its projection with the
//! overlay's denials taken out and the buffer's additions put in, both operands already row-space.
//! The type enforces that: [`MaskedSet`] has exactly one implementor outside a test build, so a
//! count cannot be taken against the pre-overlay projection, which strictly contains `M_auth` after
//! any accepted delete. That number is what a viewer is told, unmodified. The criterion
//! reads the same number and decides whether the artifact is **served at all**
//! ([decision 0075](../../../docs/decisions/0075-the-masked-count-is-an-existence-criterion.md));
//! it never rounds, floors or suppresses a value. An implementation that "applied the threshold to
//! the count" would be a different design with a different disclosure.
//!
//! **A below-criterion artifact is absent, not refused.** It does not appear, and the response
//! carries nothing that distinguishes it from an artifact that was never published — which is the
//! same indistinguishability the layer registry gives a gate-failed name.
//!
//! ## The row form is maintained, not invalidated
//!
//! A level's [`ArtifactRows`] covers the **whole** of its view's row space — base rows and every
//! flushed extent — and it is brought forward by the operations that change it rather than rebuilt
//! by them:
//!
//! - a **growth** or a **publication** applies its own delta to every held form of that level
//!   ([`ArtifactProjections::bring_forward`]) and moves the form's key with it, so the next request
//!   hits;
//! - a **flush** extends every held form of the view by the segment it published
//!   ([`ArtifactProjections::extend_flushed`]), which is what makes an ingested member count from
//!   its flush rather than from the next fold.
//!
//! - a **merge** rebases every held form of the view over the extent it published
//!   ([`ArtifactProjections::rebase_merged`]): the rows inside the merged span are cleared and
//!   the members re-projected through the one merged extent. Extent rows are the rows a merge
//!   renumbers, and this is the one publication that permutes rows a form holds.
//!
//! All three re-derive the tile index and amend the row-major column in place, because both are
//! pure functions of the form and the fold's own files describe the level as it was (**I11**:
//! nothing persisted is amended, and nothing persisted is reused past what it describes). All are
//! per `(view, layer, level)` and derived from the level's records and the row space, which is what
//! keeps them the same shared structure a built form is (**I2**).
//!
//! **A spatial level's form is the same form with another source of rows.** Its membership is
//! resolved from the level's shapes rather than projected from records (`crate::shapes`), so the
//! rows a flush or a merge brings are the segment's resolution and the rows a publication brings
//! are the new shapes' resolution over every live segment ([`SegmentRows::Resolved`],
//! [`DeltaRows::Resolved`]); everything from the union on is shared with a stored level. An
//! attribute predicate's form is the exception: its membership is the value column, evaluated per
//! request, so it takes no delta and is keyed on the geometry ([`ProjectionKey::live`]).
//!
//! **What stays base-only is the generating sets**, deliberately — see [`MembershipRows::put`].
//!
//! **[`ArtifactRows::covers`] is read at every cache hit** because a request may hold the older of
//! two live generations; on the executor every publication brings the held forms with it, so a
//! form that was current cannot fail it there. Before 2026-09-03 the form held base rows alone and
//! needed no such check; it also understated every count by the members ingested since the last
//! fold, and rebuilt the level whole — 94 to 177 s at rung 3, inside a request — at every write
//! that moved the level's version (`docs/evidence/memos/2026-09-03-post-flush-artifact-frames.md`).
//! Until 2026-09-06 a merge dropped the form and the next request paid the same projection.

use std::collections::BTreeMap;
use std::sync::{Arc, Mutex};

use croaring::Bitmap;
use rustc_hash::FxHashSet;

use tessera_lifecycle::membership::{ArtifactRecord, ArtifactStore, Attachment};
use tessera_lifecycle::wal::ParentRef;
use tessera_lifecycle::Overlay;
use tessera_types::layer::{ExistenceCriterion, LayerDeclaration, ServingLayout};
use tessera_types::{EntityId, TermId};

use tessera_store::permutation::RowSpace;

use crate::compose::MaskedSet;
use crate::containment::{ContainmentAnswers, ContainmentPartition, PartitionSource};
use crate::histogram::MaskedCounts;
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
/// Today both are built together under one [`ProjectionKey`], which is what keeps them describing
/// the same population — see [`ArtifactRows`]' one-snapshot note.
///
/// **What it deliberately does not hold.** The generating sets themselves stay in the registry:
/// the containment partition composes from them inside the same store borrow that builds this, and
/// a second entity-space copy of `Σ|G|` bitmaps would be residency spent to avoid a walk that is
/// already paid. And the proportional criterion's denominator is **not** here, because
/// [`ArtifactView::declared_size`] takes it from the row form on purpose — numerator and
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
    attachments: Vec<Option<Attachment>>,
    /// Per ordinal, this artifact's parent edges as the registry holds them — ascending by
    /// `(level, ordinal)`, empty at a root, several on a `dag` layer (`dag-hierarchies.md` §7,
    /// decision 0117). Read by the serving path to name a parent that is *also* in the response,
    /// and by nothing in [`ArtifactView`]. It is not a visibility term: see
    /// [`ArtifactRecord::parents`].
    parents: Vec<Vec<ParentRef>>,
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
    declared: Vec<Vec<u64>>,
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
    rows: Vec<Option<Arc<Bitmap>>>,
    /// Per ordinal, per rank: that content's **generating set** in row space. Pushed in lockstep
    /// with [`ArtifactRecords::declared`], which is the size the same set had in entity space.
    generating: Vec<Vec<Bitmap>>,
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
    rows_held: bool,
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
    records: ArtifactRecords,
    membership: MembershipRows,
    /// The hierarchical row-range index and the per-artifact extents — always present, because the
    /// walk is how candidacy is answered rather than an optimisation over answering it another
    /// way. A level with no artifacts has an empty one.
    index: TileIndex,
    /// The containment partition, where this level has one.
    ///
    /// `None` under any plugin but the builtin — see [`crate::containment`], whose gate is settled
    /// fail-closed — and containment then stays on the masked-count route, which asks `M_auth`
    /// itself and so cannot depend on the shape of the rule that produced it.
    partition: Option<ContainmentPartition>,
    /// **Which form this level is served in**, and the row-addressed column where that form has one
    /// (decision 0094).
    ///
    /// The two travel together and are set together, because the second is what makes the first
    /// true: a level *recorded* row-major whose column would not compose — its memberships turned
    /// out to overlap, or its file would not open — is **served** artifact-major, and this field
    /// says so. Nothing downstream ever has to ask whether the column matching the layout is
    /// present.
    layout: ServingLayout,
    column: Option<Arc<RowColumn>>,
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
    /// [`crate::compose::RowProjection`] carries. That one is a session's and is only ever
    /// extended; this one is shared, and is asked about by requests at **two** live generations at
    /// once — a session may be served one geometry behind the newest (decision 0044). A form that
    /// had to be at exactly the asker's extent count would then be rebuilt by each of the two in
    /// turn, for ever, which is a thrash and not a wrong answer. Comparing the shared prefix
    /// answers both directions and is exact for `RowProjection`'s own reason: `seg_id`s are never
    /// reused (contracts §2.1).
    base_rows: u32,
    covered: Vec<String>,
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
    inherited: Vec<(String, u32, u64)>,
}

/// What one viewport's narrowing produced, on whichever route the level's layout takes.
///
/// **Neither variant is a verdict**, and that is the property both halves share:
/// [`ArtifactView::verdict`] runs for every ordinal either of them hands back.
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
    fn put(&mut self, idx: usize, record: &ArtifactRecord) {
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
    fn put(&mut self, idx: usize, record: &ArtifactRecord, space: &RowSpace) -> Arc<Bitmap> {
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
            // **Base rows, unlike the membership above**, and the consequence is sharper than a
            // low count: a generating set that lost members in projection can never be contained,
            // so a label whose sample includes documents ingested since the last fold is withheld
            // from **everyone** until that fold. Fail-closed, and the direction this must fail in —
            // the alternative is serving content on a set that no longer names what the text was
            // derived from. Left base-only when the membership stopped being so (2026-09-03),
            // because the containment partition beside it is composed at a level version and knows
            // nothing of the geometry: widening one half alone is how the two come apart.
            .map(|v| space.project_base(&v.generated_from))
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
    fn put_generating(&mut self, idx: usize, record: &ArtifactRecord, space: &RowSpace) {
        if self.rows.len() <= idx {
            self.rows.resize_with(idx + 1, || None);
            self.generating.resize_with(idx + 1, Vec::new);
        }
        self.rows[idx] = Some(Arc::new(Bitmap::new()));
        self.generating[idx] = record
            .contents
            .iter()
            .map(|v| space.project_base(&v.generated_from))
            .collect();
    }

    /// One artifact's generating sets projected again from the record — the tick's whole arm,
    /// where a page held a leave or a fill changed which contents the artifact has. The
    /// membership is left where it is: neither route touches it.
    fn project_generating(&mut self, idx: usize, record: &ArtifactRecord, space: &RowSpace) {
        if idx >= self.generating.len() {
            return;
        }
        self.generating[idx] = record
            .contents
            .iter()
            .map(|v| space.project_base(&v.generated_from))
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
    fn absorb_transposed(&mut self, mut transposed: Vec<Bitmap>) -> bool {
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
    fn hold_no_rows(&mut self) {
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
    fn put_resolved(
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
            .map(|v| space.project_base(&v.generated_from))
            .collect();
        rows
    }

    /// The slot at `idx` takes `rows` outright. The one caller is [`ArtifactRows::inherit`], where
    /// the rows are not this artifact's own and it has nothing of its own to keep. A membership is
    /// never otherwise replaced: it grows ([`Self::or_rows`]) or it is rebased over one extent
    /// ([`Self::rebase_rows`]).
    fn put_rows(&mut self, idx: usize, rows: Bitmap) {
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
    fn or_rows(&mut self, idx: usize, rows: &Bitmap) -> bool {
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
    fn rebase_rows(&mut self, idx: usize, lo: u32, hi: u32, rows: &Bitmap) -> bool {
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
    fn live_slots(&self) -> Vec<bool> {
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
fn drawn_record<'a>(
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
fn total_rows(space: &RowSpace) -> u32 {
    u32::try_from(space.total_rows()).unwrap_or(u32::MAX)
}

/// The `seg_id` of every extent a row space carries, in order — see [`ArtifactRows::covered`].
fn covered_by(space: &RowSpace) -> Vec<String> {
    space
        .extents()
        .iter()
        .map(|extent| extent.seg_id.clone())
        .collect()
}

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

    /// Which form this level is **served** in — see [`ArtifactRows::with_column`] on why that is not
    /// always the form the manifest records.
    pub fn layout(&self) -> ServingLayout {
        self.layout
    }

    /// Whether this form's segments and `space`'s agree as far as the shorter of the two goes —
    /// the base row count equal, and one extent list a prefix of the other.
    ///
    /// **The base row count** because a different `permutation.bin` is a different corpus; a fold
    /// publishes a new prefix and [`ProjectionKey`] discriminates on that, so this is belt to that
    /// brace. **The prefix of ids** because a *merge* collapses a run of extents into one with a
    /// new id at the same `row_base` and re-sorts the rows inside it: `seg_id`s are never reused
    /// (contracts §2.1), so ids standing where they stood are the same segments and the rows in
    /// them are the same rows. That makes the comparison exact rather than a heuristic, exactly as
    /// it is for [`crate::compose::RowProjection::extends_to`].
    fn agrees_with(&self, space: &RowSpace) -> bool {
        if self.base_rows != space.base_rows() {
            return false;
        }
        let shared = self.covered.len().min(space.extent_count());
        self.covered[..shared]
            .iter()
            .zip(&space.extents()[..shared])
            .all(|(held, extent)| held == &extent.seg_id)
    }

    /// **Whether this form may answer for `space`** — the check read beside [`ProjectionKey`] at
    /// every cache hit, because nothing in that key moves when the geometry does.
    ///
    /// The form must hold **at least** every extent `space` carries, on the agreeing prefix. Short
    /// of that it would understate every artifact with a member in the segments it is missing.
    ///
    /// **Longer is served, and that is not laxity.** A form covering extents `space` does not have
    /// holds those extra rows *above* `space`'s whole row count — a flush appends, so nothing it
    /// added can collide with a row `space` addresses — and every set a request intersects it with
    /// is over `space`. The counts are identical; what the extra bits buy is that a session one
    /// geometry behind the newest (decision 0044) reads the same form rather than rebuilding it
    /// against the newest reader for ever.
    ///
    /// **On the executor this cannot be false for a form that was current.** Every geometry
    /// publication brings the held forms with it before the swap: a flush extends them
    /// ([`ArtifactProjections::extend_flushed`]) and a merge rebases the span it renumbered
    /// ([`ArtifactProjections::rebase_merged`]), so a form that agreed with the outgoing
    /// generation agrees with the incoming one, and both entry points `debug_assert` that. What
    /// can still reach either is a form a request built against a generation that was superseded
    /// while it built and inserted afterwards; that form never agreed with the outgoing
    /// generation, is dropped with a `warn`, and costs the next request naming the level a
    /// projection. The request path reads this at every hit for that reason and one more: a
    /// request may itself hold the older of two live generations.
    pub fn covers(&self, space: &RowSpace) -> bool {
        self.covered.len() >= space.extent_count() && self.agrees_with(space)
    }

    /// Whether [`Self::extend_by`] over `space` would be exact — i.e. whether `space` **appends**
    /// to the row space this form holds rather than permuting it. [`Self::covers`] read the other
    /// way round, which is what a flush does and a merge does not.
    fn extends_to(&self, space: &RowSpace) -> bool {
        self.covered.len() <= space.extent_count() && self.agrees_with(space)
    }

    fn covering(&mut self, space: &RowSpace) {
        self.base_rows = space.base_rows();
        self.covered = covered_by(space);
    }

    /// **The rows an accepted growth adds, unioned into the ordinal that grew** — the delta that
    /// keeps this form the form of the level the store now holds instead of a form the next
    /// request has to build again.
    ///
    /// `joining` is entity space and is projected through the whole of `space`, base and extents
    /// alike, because that is what this form's memberships are (see [`MembershipRows::put`]). An
    /// entity still in the commit buffer has no row and projects to nothing; it reaches the form
    /// at its flush, through [`Self::extend_by`].
    ///
    /// A hole takes nothing, on [`MembershipRows::or_rows`]' rule.
    fn grow_rows(&mut self, ordinal: u32, joining: &Bitmap, space: &RowSpace) -> Bitmap {
        let rows = space.project(joining);
        let held = self.membership.get(ordinal).cloned().unwrap_or_default();
        // **What this row form did not already hold** — the rows the column has to gain, and no
        // others. A member joining an artifact it is already in adds nothing anywhere.
        //
        // **A column-only form holds none of them, so every projected row is offered**, which is a
        // superset of the rows the column gains and never a subset: `RowColumn::amend` skips a pair
        // the column already carries, so the counts it keeps do not double, and the extent below is
        // widened by rows that were already inside it.
        let fresh = rows.andnot(&held);
        self.membership.or_rows(ordinal as usize, &rows);
        fresh
    }

    /// **One newly published artifact placed at its ordinal** — records, membership and generating
    /// sets, exactly as [`Self::build_over`]'s walk would have placed it.
    ///
    /// A publication only ever appends ordinals (`LayerRegistry::prepare_artifacts` claims from a
    /// dense cursor), so this widens the form and rewrites nothing already in it.
    fn publish_at(
        &mut self,
        ordinal: u32,
        record: &ArtifactRecord,
        space: &RowSpace,
    ) -> Arc<Bitmap> {
        let idx = ordinal as usize;
        self.records.put(idx, record);
        self.membership.put(idx, record, space)
    }

    /// [`Self::publish_at`] for a membership **resolved elsewhere** — a spatial level's new shape,
    /// resolved over every live segment with the row bases applied — exactly as
    /// [`Self::build_resolved`]'s walk would have placed it.
    fn publish_resolved(
        &mut self,
        ordinal: u32,
        record: &ArtifactRecord,
        rows: Bitmap,
        space: &RowSpace,
    ) -> Arc<Bitmap> {
        let idx = ordinal as usize;
        self.records.put(idx, record);
        self.membership.put_resolved(idx, record, rows, space)
    }

    /// **One generating set unioned with the entities a page joined to it** — the fast arm of the
    /// tick's publication, for a page holding no leave (`ingest.md` §1.1, §4.1).
    ///
    /// Base rows, as a generating set's are (`MembershipRows::put`): a set whose members reach
    /// outside the base projects short and can never be contained, which is the direction this
    /// must fail in and is unchanged by a join arriving here rather than at a build.
    ///
    /// `false` where the ordinal is a hole or holds no content at that rank — neither is damage: a
    /// fold retires an artifact and withdraws a content, and a page prepared before one is a page
    /// the store applied to nothing.
    fn grow_generating(
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
        set.or_inplace(&space.project_base(joining));
        true
    }

    /// **One artifact's records entry read again, and its operators re-derived where a page held a
    /// leave** — the whole arm of the tick's publication (`ingest.md` §1.1, §4.1).
    ///
    /// The records entry is always taken, because it carries the stored cardinality a page moved
    /// and the fixed parts a fill supplied; the operators are re-projected from entity truth where
    /// `whole` says so. The membership is untouched: neither a page nor a fill changes it.
    fn refresh_sets(
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
        self.records.put(idx, record);
        if whole {
            self.membership.project_generating(idx, record, space);
        }
    }

    /// **Every artifact's membership extended by the extents this form does not yet cover** — what
    /// a flush does to a stored level's held form.
    ///
    /// The rows a segment publishes are the rows that segment's entities occupy, so the extension
    /// is `project_extents_from(members, covered)` per artifact: disjoint from everything
    /// already held, because an extent's rows begin exactly where row space ended, which is what
    /// makes the union exact rather than a superset — [`RowSpace::project`]'s own argument, read
    /// one segment at a time.
    ///
    /// **Callers check [`Self::extends_to`] first.** This does not, for
    /// [`crate::compose::RowProjection::extend`]'s reason: the answer decides whether the caller
    /// brings the form forward at all, and re-deriving it here would be a second place to get it
    /// wrong.
    fn extend_by<'a>(
        &mut self,
        artifacts: impl Iterator<Item = (u32, &'a ArtifactRecord)>,
        space: &RowSpace,
    ) -> (Vec<(u32, u32)>, u64) {
        let from = self.covered.len();
        let mut added = Vec::new();
        let mut taken = 0u64;
        for (ordinal, record) in artifacts {
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

    /// [`Self::extend_by`] for a spatial level: `piece` is the new segment's resolution, one
    /// segment-local row set per ordinal, taken at `row_base`. Parallel to the level's ordinals as
    /// the shapes were held when the segment was resolved; a hole takes nothing.
    fn extend_by_resolved(
        &mut self,
        piece: &[Option<Bitmap>],
        row_base: u32,
    ) -> (Vec<(u32, u32)>, u64) {
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
    fn rebase_span_resolved(
        &mut self,
        piece: &[Option<Bitmap>],
        space: &RowSpace,
        start: usize,
    ) -> (u32, u32, Vec<(u32, u32)>, u64) {
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

    /// **Carry the two derived structures over the amendment** — the tile index, re-derived, and
    /// the row-major column, amended at `added` and nowhere else.
    ///
    /// **The index is re-derived and the column is not**, and the asymmetry is what each costs. An
    /// extent is `minimum` and `maximum` per artifact, which is O(1) a bitmap and the walk
    /// `blocks_per_artifact` already makes at every build; a column is one entry per membership
    /// entry, which at rung 3's `mesh/descriptors` is 1.66×10⁹ of them and ~100 s **on the
    /// executor thread**, where it blocks every ingest and every deny. So the column takes the
    /// delta — `added` is `(row, ordinal)` for the rows this amendment gave that artifact and no
    /// others, and [`RowColumn::amend`] shares the pack rather than reading it. The column is
    /// amended in place: `Arc::make_mut` copies it only where a request is still reading this
    /// form, and then copies the amendment and the counts (and a live tail's labels), never the pack.
    ///
    /// Neither is re-adopted from the prefix, and **I11** is why: the fold's files describe the
    /// level as it was before the amendment, and a *narrow* extent settles an artifact whose
    /// members reach outside the viewport.
    ///
    /// `true` where a level that *was* served row-major no longer has a column: a growth can make
    /// two memberships overlap, which a label column cannot express. The level then serves
    /// artifact-major, which answers identically, and the caller says so — the one place the
    /// recorded layout and the served one may differ, reached by [`Self::with_column`]'s route.
    fn amend_derived(&mut self, added: &[(u32, u32)], row_count: u32) -> bool {
        // **A column-only form has no row form to re-derive from**, so the extents take the same
        // delta the column does: `added` is every `(row, ordinal)` this amendment gave the level,
        // and widening by it is exact where rows are only added ([`TileIndex::amend`]).
        if self.membership.rows_held() {
            self.index = TileIndex::build(&self.membership, row_count);
        } else {
            self.index
                .amend(added, self.membership.len() as u32, row_count);
        }
        let Some(column) = &mut self.column else {
            return false;
        };
        if Arc::make_mut(column).amend(added, row_count) {
            return false;
        }
        self.lose_column();
        true
    }

    /// **Every artifact's membership rebased over the extent at `start`** — what a row-space merge
    /// does to a held form. The merged extent stands where the run it consumed stood, at the same
    /// `row_base` with the same row count, and the rows inside it are the consumed segments' rows
    /// in another order; so each artifact's bits in that span are cleared and its members
    /// re-projected through the one extent, [`Self::extend_by`]'s projection asked of one extent
    /// rather than of every extent from a point. Rows below the span are base rows or earlier
    /// extents' rows, which a merge does not move; rows above it belong to later extents, whose
    /// `row_base` a merge preserves.
    ///
    /// The cost is one `remove_range` per artifact and the members inside the merged extent's
    /// entity range — the work the level would otherwise pay as a whole projection on the next
    /// request that named it (`probes/2026-09-05-merge-arm/`: 108 s at rung 3, shed).
    ///
    /// Returns `(lo, hi, added)`: the span cleared and the `(row, ordinal)` pairs the column
    /// takes back, on [`Self::extend_by`]'s terms.
    fn rebase_span<'a>(
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

    /// [`Self::amend_derived`] for a rebase: the tile index re-derived, the column's labels in
    /// `lo..hi` given up and `added` taken in their place ([`RowColumn::rebase`]). `true` on that
    /// method's terms.
    fn rebase_derived(&mut self, lo: u32, hi: u32, added: &[(u32, u32)], row_count: u32) -> bool {
        // [`Self::amend_derived`]'s rule; the merge is the one amendment whose widening is a
        // superset rather than an equality — see [`TileIndex::amend`].
        if self.membership.rows_held() {
            self.index = TileIndex::build(&self.membership, row_count);
        } else {
            self.index
                .amend(added, self.membership.len() as u32, row_count);
        }
        let Some(column) = &mut self.column else {
            return false;
        };
        if Arc::make_mut(column).rebase(lo, hi, added, row_count) {
            return false;
        }
        self.lose_column();
        true
    }

    /// **The refused amendment's posture, on a form that holds its own bitmaps**: the level goes
    /// back to the artifact-major route, which answers identically.
    ///
    /// A form that holds no bitmaps keeps its column here — there is nothing to fall back to — and
    /// the caller recomposes it in the list form ([`Self::recompose_as_list`]) before anything
    /// reads it. That is why this is not simply `self.column = None`.
    fn lose_column(&mut self) {
        if self.membership.rows_held() {
            self.layout = ServingLayout::ArtifactMajor;
            self.column = None;
        }
    }

    /// **Take the list form from the column this level already holds**, after an amendment the
    /// label form could not express — see [`RowColumn::recompose_as_list`], which is where the
    /// bound is argued.
    ///
    /// The extents are re-derived from the new column's own bytes, exactly as they were at the
    /// build: the form's one membership is the column, and the two may not be allowed to
    /// disagree.
    ///
    /// `false` where the composition failed, which is an I/O failure and not a shape: the caller
    /// drops the form and the next request projects the level whole.
    fn recompose_as_list(&mut self, added: &[(u32, u32)], scratch: &std::path::Path) -> bool {
        let Some(column) = self.column.as_deref() else {
            return false;
        };
        let row_count = self.index.row_count().max(column.row_count());
        let Some(listed) = column.recompose_as_list(added, self.base_rows, row_count, scratch)
        else {
            return false;
        };
        let live = self.membership.live_slots();
        self.index = TileIndex::of_bytes(tessera_store::membership::pack_tile_index(
            row_count,
            &listed.extents(&live),
        ));
        self.layout = listed.layout();
        self.column = Some(Arc::new(listed));
        true
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
    fn inherit(
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
    fn inherited_current(&self, store: &ArtifactStore) -> bool {
        self.inherited
            .iter()
            .all(|(layer, level, version)| store.level_version(layer, *level) == *version)
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
    /// [`ArtifactView::verdict`] reads it first.
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
    /// (the review's finding 1). Nothing here returns a verdict — [`ArtifactView::verdict`] still
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
            // Row space rather than entity space, and exact for the same reason the expression is:
            // every member of a set that cleared the projection check above has a base row, and
            // `row_of` is injective, so `projected ∩ denied_rows = ∅` iff `G ∩ denied = ∅`.
            if denied.intersect(rows) {
                continue;
            }
            return Some(Containment::Satisfied(i as u32));
        }
        Some(Containment::Unsatisfied)
    }
}

/// **Where a predicate level's membership comes from**, resolved against this generation — the
/// pieces [`ArtifactProjections::get_or_build`] needs and cannot reach itself.
///
/// **`None` is an enumerated level**, and that is not a fallback: such a level's membership is
/// stored, so there is no rule to evaluate and nothing here to supply.
pub enum PredicateSource<'a> {
    /// `membership = { attribute = f }` — the indexed column `f`, addressed by entity.
    Attribute(AttributeSource<'a>),
    /// `membership = "spatial"` — the level's held shapes and the segments whose resolutions the
    /// form is assembled from, once (`crate::shapes`).
    Spatial(SpatialSource<'a>),
}

/// The indexed column an attribute layer's predicate reads, and the rule that turns one of its
/// values into one of the layer's artifacts.
pub struct AttributeSource<'a> {
    /// The column's value layers, base first (`crate::filter::ValueLayers`). The **base** answers
    /// for the rows the row space's base covers; the **extents** answer for everything a flush has
    /// published since, which is what makes an ingested point count on the next request.
    pub values: crate::filter::ValueLayers<'a>,
    /// The code an artifact's key stands for — a vocabulary binding where the column has one, and
    /// the key's own decimal spelling where it has not.
    ///
    /// **The inverse of the rule the mint uses** (`tessera_types::layer::attribute_value_key`), and
    /// it is a closure rather than a map because the two callers hold different things: the build
    /// holds a schema and the serving path holds a live generation's bindings.
    pub code_of_key: &'a dyn Fn(&str) -> Option<u32>,
}

/// The held structures a spatial level's form is assembled from when it is built, and what it is
/// assembled over. Built once; from then on the form is maintained by the publications that move
/// it, as a stored level's is.
pub struct SpatialSource<'a> {
    /// The level's shapes, index and staged pieces, at this generation's level version.
    pub level: Arc<crate::shapes::ShapeLevel>,
    /// This generation's segments and their row bases, base first.
    pub segments: &'a [(&'a tessera_store::read::SegmentData, u32)],
    /// The generation's whole row count, base and extents — what the joined form is sized to.
    pub total_rows: u32,
}

/// What a cached [`ArtifactRows`] was built from. **Every term is a reason the projection would be
/// wrong**, and a mismatch on any of them rebuilds:
///
/// - the **prefix**, because a fold renumbers the base row space wholesale, so a projection built
///   over the old one names other people's documents;
/// - the **view**, because row space is per view;
/// - the **level's version**, because a publication adds memberships the projection has never
///   seen — and a cached projection that silently omitted them would serve a level with its
///   newest clusters absent, indistinguishable from clusters that failed their criterion.
///
/// **The level's version and not the store's**, which is what this carried until the scale
/// campaign measured the difference (`design/artifact-serving-at-scale.md` §8.1): a store-wide
/// counter makes one suppression, one growth or one publication *anywhere* invalidate every
/// level's form in every view — 138 s of rebuild at 10⁷ artifacts over 10⁹ rows, so under any
/// read-write load the cache never survives to be used. The narrower key is sound because the
/// build reads exactly two things: the records of one `(layer, level)`, which is what that level's
/// version counts, and the view's row space, which is fixed by the two terms above it.
///
/// **The segments version is deliberately not a term, and it is not the row space's only guard.**
/// A stored level's form covers extent rows, so a flush and a merge both matter to it — but
/// neither is answered by rebuilding at a version. A flush **extends** the form
/// ([`ArtifactProjections::extend_flushed`]), which keying on the segments version would turn into
/// a whole-level projection per flush for a set of bits that mostly did not move; a merge permutes
/// rows the form holds and is caught by [`ArtifactRows::covers`], read beside this key at every
/// hit. The one operation that renumbers the base is the fold, and a fold publishes a new prefix.
#[derive(Debug, Clone, PartialEq, Eq)]
struct ProjectionKey {
    prefix: String,
    view: String,
    level_version: u64,
    /// **The geometry an *attribute* predicate's membership was evaluated against, and `0` for
    /// every other level.**
    ///
    /// A stored membership and a spatial one are sets the form can be **extended** by the segment
    /// a flush publishes and **rebased** over the extent a merge publishes
    /// ([`ArtifactProjections::extend_flushed`], [`ArtifactProjections::rebase_merged`]), which
    /// is why `segments_version` is not a term above. An attribute predicate's membership is the
    /// value column: its live tail covers the rows a flush appended and is read per request, so
    /// there is no delta to take and the form is rebuilt when the geometry moves — once per flush
    /// rather than once per fold, over a column that is four bytes a row.
    ///
    /// **Zero rather than an `Option`**, because a level either evaluates a column or it does not:
    /// an enumerated or a spatial level filed under a geometry would rebuild on every flush what a
    /// walk of one extent brings forward.
    live: u64,
}

impl ProjectionKey {
    /// Whether a form held under this key answers for `wanted` — the level as last published,
    /// which may be up to a tick behind the store (`ingest.md` §1.3, §10 ruling 6).
    ///
    /// Every term but the level version is an equality: a form under another prefix, of another
    /// view, or of an attribute predicate whose value column the geometry has moved, describes
    /// something else. The level version is a floor, because the deltas the store has taken since
    /// reach the form at the tick and until then the form is what was published — stale in the
    /// direction that understates.
    fn stale_form_of(&self, wanted: &ProjectionKey) -> bool {
        self.prefix == wanted.prefix
            && self.view == wanted.view
            && self.live == wanted.live
            && self.level_version <= wanted.level_version
    }
}

/// One row-space projection per `(view, layer, level)`, rebuilt when its [`ProjectionKey`] moves.
///
/// **Costly to build and therefore never built on a request that can reuse one.**
/// `RowSpace::project` decodes a whole membership; at corpus scale that is the "seconds, not
/// milliseconds" cost `RowProjection` carries the same warning about. This is paid at the first
/// request after a generation move or a publication, and by nothing else.
///
/// **Replace-on-mismatch, not an LRU.** The key names the only generation a projection is valid
/// for, so a stale entry has no value to retain — keeping one would be keeping a wrong answer
/// warm. The map is therefore bounded by the number of live `(view, layer, level)` triples rather
/// than by a capacity anyone has to tune.
/// `(view, layer, level)` — what one cached projection is *for*, as against the
/// [`ProjectionKey`] that says when it stops being valid.
type LevelAddress = (String, String, u32);

/// `(layer, level)` — what one cached partition is *for*.
///
/// **No view**, because the expression is over terms and no row space is involved in it; and **no
/// prefix**, for the reason [`LevelAddress`] carries none: the prefix is a *validity* term, so
/// putting it in the address would leave every fold's entries behind for the process's life
/// instead of replacing them.
type PartitionAddress = (String, u32);

/// What a cached [`ContainmentPartition`] was composed from. The view is deliberately absent — see
/// [`ArtifactProjections::partitions_held`] — so the terms are the prefix, which fixes the
/// postings, and the level's version, which fixes the records.
#[derive(Debug, Clone, PartialEq, Eq)]
struct PartitionKey {
    prefix: String,
    level_version: u64,
}

/// `(view, layer, level)` — what one fold-written tile index is *for*.
///
/// **The view is here and not in [`PartitionAddress`]**, and that is the whole difference between
/// the two structures: a containment expression names entities' terms, so no row space is involved
/// in it and one file answers for every view; an extent is a pair of **rows**, so it answers for
/// exactly the view it was projected through.
type IndexAddress = (String, String, u32);

/// What an adopted [`TileIndex`] was projected under — the same two validity terms
/// [`PartitionKey`] carries, and for the same reasons. The prefix fixes the base row space (a fold
/// renumbers it wholesale; a flush and a merge leave it alone, which is why the segments version is
/// not a term — [`ProjectionKey`] argues it); the level's version fixes the memberships.
#[derive(Debug, Clone, PartialEq, Eq)]
struct IndexKey {
    prefix: String,
    level_version: u64,
}

/// **What one accepted write did to a level**, held by the executor until the next flush tick and
/// applied to the level's row forms there ([`ArtifactProjections::publish`], `ingest.md` §1.3 and
/// §10, ruling 6).
///
/// Entity space and a delta, exactly as the log record carries it — a row-space set would be a
/// frozen projection, and this is applied once per view over that view's own row space.
///
/// **`before` is the level version this write followed**, and it is what lets a form be brought
/// from wherever it stands to the store's present: the deltas of an interval carry consecutive
/// versions, so a form at version *v* takes the ones from *v* on and none of the ones it already
/// holds.
#[derive(Debug)]
pub struct LevelDelta {
    pub before: u64,
    pub kind: DeltaKind,
}

/// The four shapes a [`LevelDelta`] takes, one per route by which artifact state enters.
#[derive(Debug)]
pub enum DeltaKind {
    /// One `WalRecord::ArtifactGrow` decoded: `(ordinal, the entities joining)` per membership
    /// it grew, which is the same delta `ArtifactStore::grow` applied to the records, and one page
    /// per generating set it moved. One record carries both because one record moves the level's
    /// version once.
    Grown {
        joins: Vec<(u32, Bitmap)>,
        pages: Vec<SetPage>,
    },
    /// The ordinals a publication claimed. The records themselves are read back from the store,
    /// which has already applied them.
    Published(Vec<u32>),
    /// The ordinals a fill changed. A fill supplies a fixed part — a parent list, an attachment, a
    /// shape or a content's values — so what the form takes from it is the record again, not rows:
    /// the membership is untouched and the ordinal's records entry and generating sets are read
    /// from the store.
    Filled(Vec<u32>),
}

/// One page of one content's generating set, as the row forms take it (`ingest.md` §1.1).
#[derive(Debug)]
pub struct SetPage {
    pub ordinal: u32,
    pub rank: u16,
    /// The entities joining. Unioned into the served operator where the page holds no leave.
    pub joining: Bitmap,
    /// **Whether this page re-derives the artifact's operators whole from entity truth**, which a
    /// page holding any leave does and a page of joins alone does not (`ingest.md` §1.1, §4.1). A
    /// union cannot express a leave, and a cardinality moved down against an operator that still
    /// holds the leaver is a pair that was never derived together. A page that empties the set
    /// withdraws the content, which moves every rank above it, and that is re-derived for the same
    /// reason.
    pub whole: bool,
}

/// **Where the rows of a write's delta come from**, per level — what
/// [`ArtifactProjections::bring_forward`] is told beside the delta.
///
/// `None` at the call is an attribute predicate, whose membership is the value column and takes
/// no delta.
pub enum DeltaRows<'a> {
    /// A stored membership: the delta's rows are the records' members, projected through the
    /// view's row space.
    Projected,
    /// A spatial membership: the rows of the ordinal asked for, resolved from its shape over every
    /// live segment with the row bases applied. A growth never reaches a spatial level (the
    /// registry refuses one), so this is asked for a publication's new ordinals only.
    Resolved(&'a dyn Fn(u32) -> Bitmap),
}

/// **Where a level's rows come from when a geometry publication brings its held form forward** —
/// what [`ArtifactProjections::extend_flushed`] and [`ArtifactProjections::rebase_merged`] are
/// told per `(layer, level)`. `None` is a level that takes no delta here: an attribute predicate.
pub enum SegmentRows {
    /// A stored membership: the segment's rows are projected from the records through its extent.
    Projected,
    /// A spatial membership: the segment resolved against the level's shapes, segment-local rows
    /// per ordinal, parallel to the level's ordinals.
    Resolved(Arc<Vec<Option<Bitmap>>>),
}

/// One held row form, what it describes and how far its rows have been brought.
#[derive(Debug)]
struct Held {
    key: ProjectionKey,
    /// The `segments_version` the form's rows were last brought to — the generation it was built
    /// against, or the last one whose publication extended or rebased it on the executor.
    ///
    /// **What keeps a request from undoing the executor's work.** A request builds a form against
    /// the generation it loaded and inserts it when the build ends; at rung 3 a build is tens of
    /// seconds and the tick is ninety, so a build that straddles a flush or a merge is the
    /// ordinary case rather than a race. Inserted unconditionally, that form would replace one the
    /// publication had just extended or rebased with one that is a segment short or holds the
    /// consumed segments' rows — and the next request would find `covers` false and project the
    /// level again. So an insert keeps whichever of the two is at the later version
    /// ([`ArtifactProjections::insert_newest`]).
    at: u64,
    rows: Arc<ArtifactRows>,
}

/// The engine cache directory's subdirectory for row-column compositions
/// (`tessera_store::derived::project_row_column`). Its own directory rather than the cache root, so
/// the open-time sweep cannot reach the fragment cache beside it.
pub const ROW_COLUMN_SCRATCH_DIR: &str = "row-columns";

// **`Default` is [`ArtifactProjections::new`]'s own scaffold and not a constructor.** Every field
// but the scratch path is an empty cache, and the scratch path a default gives is empty, which is a
// directory no composition can write through. `new` fills it in and nothing else calls `default`; a
// caller that did would get a projection that declined every level it was asked for.
#[derive(Debug, Default)]
pub struct ArtifactProjections {
    cached: Mutex<BTreeMap<LevelAddress, Held>>,
    /// The containment partitions, keyed **without the view**.
    ///
    /// **The expression is view-independent, and composing it is not cheap.** It names entities'
    /// terms, so two views of the same level compose the same table — but composing it walks every
    /// term's posting once (`crate::containment`), which at the demo corpus's 54,794 signatures is
    /// the dear half of a level's build. Held here, a second view of a level pays nothing for it.
    ///
    /// **What stays per view is the projection-loss test**, and that is why this cache can be
    /// narrower than the one above rather than replacing it: a generating set that lost a member on
    /// the way into *this* view's row space can never be contained, and that question is asked of
    /// the row form at serving time.
    partitions_held: Mutex<BTreeMap<PartitionAddress, (PartitionKey, ContainmentPartition)>>,
    /// The fold-written tile indexes adopted at open, waiting for the level's first request to
    /// claim one.
    ///
    /// **Claimed once and then dropped**, which is the difference from the map above it. A
    /// containment partition is *held* because two views of a level share it and a row form does
    /// not; an index belongs to one view, so once that view's row form has taken it there is
    /// nothing left for a second reader — and keeping a second `Arc` to eighty megabytes per level
    /// for the process's life is the retention bug `forget` exists to fix, one map along.
    indexes_held: Mutex<BTreeMap<IndexAddress, (IndexKey, TileIndex)>>,
    /// The fold-written row-major columns adopted at open, waiting for the level's first request to
    /// claim one.
    ///
    /// **[`Self::indexes_held`]'s map, one structure along**, with the same address and the same two
    /// validity terms: a column is addressed by **row**, so it answers for exactly the view whose
    /// row space it was written over, and the level's version is what says whether it still
    /// describes that level. Claimed once and then dropped, because a column belongs to one view's
    /// row form.
    columns_held: Mutex<BTreeMap<IndexAddress, (IndexKey, RowColumn)>>,
    /// The **base** half of an attribute predicate's row column, per `(view, layer, level)`.
    ///
    /// **Held rather than claimed**, which is the difference from [`Self::columns_held`]: a
    /// fold-written column is taken once and dropped, because the form that took it holds the only
    /// copy. A predicate's base is taken again at *every flush* — the form above it is rebuilt when
    /// the geometry moves and the base is not — so it stays here, replaced when its coordinate
    /// moves, and `with_tail` shares it rather than copying four bytes a row per flush.
    predicate_bases: Mutex<BTreeMap<IndexAddress, (IndexKey, Arc<RowColumn>)>>,
    /// How many forms this has built since the engine opened. **The cadence, counted** — what
    /// §8.1 is about is not the cost of one build but how many a write provokes, and that is a
    /// number nothing reported until the grain changed. Read by the fold's own log line and by
    /// [`crate::Engine::artifact_cache_builds`].
    builds: std::sync::atomic::AtomicU64,
    /// How many containment partitions this has **composed** since the engine opened. **The gate,
    /// counted** — under a foreign plugin it stays at zero while `builds` climbs, which is what
    /// makes *the partition declined everywhere* distinguishable from *the partition was never
    /// asked for*. It is not `builds`' twin even under the builtin plugin: a second view of a
    /// level builds a second row form and reuses the one partition, which is the whole point of
    /// [`Self::partitions_held`]. Operator plane only; it names no artifact and no principal.
    partitions: std::sync::atomic::AtomicU64,
    /// How many partitions this **adopted** from the prefix at open rather than composing — the
    /// other half of the same gauge. A deployment that folded and restarted should see this at the
    /// number of levels it holds and [`Self::partitions`] at zero; seeing it at zero and the other
    /// climbing says every coordinate was rejected, which is correct but is the expensive answer
    /// and an operator has no other way to notice it.
    adopted: std::sync::atomic::AtomicU64,
    /// How many fold-written tile indexes this **claimed** from the prefix rather than deriving —
    /// the same pair of gauges one structure along, and read the same way. Counted at the claim
    /// rather than at the adoption, because an entry the manifest named and no request ever asked
    /// for saved nothing.
    indexes_adopted: std::sync::atomic::AtomicU64,
    /// How many fold-written row-major columns this **claimed** from the prefix rather than
    /// composing — the same pair of gauges one structure along, and read the same way.
    columns_adopted: std::sync::atomic::AtomicU64,
    /// How many levels were **recorded** row-major and are being **served** artifact-major: their
    /// memberships turned out to overlap, or the fold's file would not open.
    ///
    /// **The number that says a pin is wrong**, and an operator has no other way to see it: both
    /// layouts answer identically, so a level that fell back is correct and merely slower than the
    /// operator asked for. Counted per build rather than per request, and it names no artifact and
    /// no principal.
    fallbacks: std::sync::atomic::AtomicU64,
    /// How many row-major columns this **composed** rather than claiming from the prefix —
    /// [`Self::columns_adopted`]'s other half, read the same way. A deployment that folded and
    /// restarted should see this at zero and the adopted gauge at the number of row-major levels it
    /// holds; seeing the reverse says every coordinate was rejected, which is correct and is the
    /// expensive answer.
    ///
    /// **A predicate level's base column counts here too, and never has an adopted twin**: its
    /// labels come from the value column the predicate names rather than from a stored membership,
    /// so the fold writes no file for it and there is nothing for a reader to claim. Such a level
    /// contributes one composition per prefix per view, not one per flush — the base is cached in
    /// [`Self::predicate_bases`] and the tail above it is what the geometry moves.
    columns_composed: std::sync::atomic::AtomicU64,
    /// Where a composition's partition buckets and the column it writes live — the deployment's
    /// cache directory, never the bundle.
    ///
    /// **A column is composed front to back into a file** and every pair it routes goes through a
    /// disk partition (`tessera_store::derived::project_row_column`), so the engine needs scratch
    /// of its own for the same reason a build needs `.build-tmp/`. The cache directory is where a
    /// derived, undigested, rebuilt-every-open file belongs — contracts §2.1 fixes what a bundle
    /// contains and this is not part of it, which is the argument the suggestion indexes already
    /// make one directory along.
    scratch: std::path::PathBuf,
}

/// **Whether this layer's derived content is an accumulation over the mask** — a centroid or a
/// bounding box, which are the two [`crate::histogram::MaskedGeometry`] answers.
///
/// A layer that declares neither pays nothing for one: the accumulation reads a grid position per
/// visible row and holds 36 B an ordinal, and a layer serving a count alone has no use for either.
pub fn derives_accumulated_geometry(declaration: &LayerDeclaration) -> bool {
    declaration
        .content
        .computed
        .iter()
        .filter_map(|name| crate::derived::ComputedProperty::parse(name))
        .any(|p| {
            matches!(
                p,
                crate::derived::ComputedProperty::Centroid | crate::derived::ComputedProperty::Box
            )
        })
}

/// **Whether a level of this layer may be served from its column alone** — the one place the rule
/// is stated, because two callers deciding it differently would flip the level's form between
/// requests.
///
/// ⊘ **A layer that derives a `hull` is excluded.** Every other per-artifact answer over a
/// column-only level is an accumulation — a count, a sum, a minimum and a maximum — which one pass
/// over the mask produces for every artifact at once ([`crate::histogram::MaskedGeometry`]). A hull
/// is not: it is a function of the member *positions* themselves, so it needs one artifact's rows
/// materialised, and on a scattered artifact that walk is the whole visible set. Such a level keeps
/// the artifact-major form and pays its residency.
pub fn serves_column_only(declaration: &LayerDeclaration) -> bool {
    !declaration
        .content
        .computed
        .iter()
        .filter_map(|name| crate::derived::ComputedProperty::parse(name))
        .any(|p| p == crate::derived::ComputedProperty::Hull)
}

impl ArtifactProjections {
    /// `scratch` is the directory compositions write through — see [`Self::scratch`].
    pub fn new(scratch: impl Into<std::path::PathBuf>) -> Self {
        Self {
            scratch: scratch.into(),
            ..Self::default()
        }
    }

    /// See [`Self::scratch`].
    pub(crate) fn scratch(&self) -> &std::path::Path {
        &self.scratch
    }

    /// See [`Self::builds`].
    pub fn builds(&self) -> u64 {
        self.builds.load(std::sync::atomic::Ordering::Relaxed)
    }

    /// See [`Self::partitions`].
    pub fn partitions(&self) -> u64 {
        self.partitions.load(std::sync::atomic::Ordering::Relaxed)
    }

    /// See [`Self::adopted`].
    pub fn adopted(&self) -> u64 {
        self.adopted.load(std::sync::atomic::Ordering::Relaxed)
    }

    /// See [`Self::indexes_adopted`].
    pub fn indexes_adopted(&self) -> u64 {
        self.indexes_adopted
            .load(std::sync::atomic::Ordering::Relaxed)
    }

    /// See [`Self::columns_adopted`].
    pub fn columns_adopted(&self) -> u64 {
        self.columns_adopted
            .load(std::sync::atomic::Ordering::Relaxed)
    }

    /// See [`Self::fallbacks`].
    pub fn layout_fallbacks(&self) -> u64 {
        self.fallbacks.load(std::sync::atomic::Ordering::Relaxed)
    }

    /// See [`Self::columns_composed`].
    pub fn columns_composed(&self) -> u64 {
        self.columns_composed
            .load(std::sync::atomic::Ordering::Relaxed)
    }

    /// Take the fold-written partitions this prefix's manifests name, for every level whose
    /// coordinate still holds.
    ///
    /// **The coordinate is the whole rule and there is no weaker form of it.** A partition is a
    /// pure function of a level's records and the prefix's postings, so an entry describes the
    /// level at exactly one version; this adopts it where the level it seeded — after the manifest
    /// *and* after everything the WAL replayed over it — is at that same version, and drops it
    /// otherwise. Growth shrinks nothing and publication only adds, so a stale partition answers
    /// containment for a generating set that has since grown, and growth makes containment
    /// **harder** — the stale answer is the permissive one, on the one test **I3** exists to make
    /// conservative.
    ///
    /// **Every failure is a drop, not an error.** A file that will not map, a file whose framing
    /// refuses, a coordinate that has moved: each means *recompose this level on first use*, which
    /// is the answer every request took before the fold wrote anything. Refusing to open the
    /// engine over a derived structure that has a correct fallback would be a refusal outside the
    /// disclosure surface.
    pub fn adopt_all(
        &self,
        prefix_dir: &std::path::Path,
        prefix: &str,
        extents: &[tessera_store::manifest::ContainmentExtent],
        store: &ArtifactStore,
    ) {
        // **Everything held for another prefix leaves here.** A claim leaves an entry held for a
        // prefix other than the one it asks under, so that a request still building on the
        // outgoing generation cannot remove what this adoption inserts ([`Self::claim_index`]);
        // what keeps such an entry from outliving its prefix is this purge, at the adoption that
        // supersedes it.
        self.partitions_held
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .retain(|_, (key, _)| key.prefix == prefix);
        for extent in extents {
            let level_version = store.level_version(&extent.layer, extent.level);
            if level_version != extent.level_version {
                tracing::info!(
                    layer = %extent.layer,
                    level = extent.level,
                    composed_at = extent.level_version,
                    now = level_version,
                    "a fold-written containment partition is not adopted: the level has moved                      since it was composed, so it is recomposed on first use"
                );
                continue;
            }
            let path = prefix_dir.join(&extent.path);
            let partition = match ContainmentPartition::open(&path) {
                Ok(partition) => partition,
                Err(error) => {
                    // Loud, because this one is a fault rather than a cadence: the manifest names
                    // a file the prefix should hold and it did not open.
                    tracing::error!(
                        layer = %extent.layer,
                        level = extent.level,
                        path = %extent.path,
                        %error,
                        "ALARM: a containment partition named by the manifest would not open;                          containment is correct and the level recomposes on first use"
                    );
                    continue;
                }
            };
            self.adopted
                .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
            self.partitions_held
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .insert(
                    (extent.layer.clone(), extent.level),
                    (
                        PartitionKey {
                            prefix: prefix.to_string(),
                            level_version,
                        },
                        partition,
                    ),
                );
        }
    }

    /// Take the fold-written tile indexes this prefix's manifests name, for every
    /// `(view, layer, level)` whose coordinate still holds.
    ///
    /// **[`Self::adopt_all`]'s rule with a view on it, and the direction of the mistake is the
    /// mirror image.** A stale containment partition answers containment for a generating set that
    /// has since grown, which is the permissive direction. A stale index is **narrow**: a growth
    /// added members the extents do not reach, so an artifact is settled whose membership is not
    /// inside the viewport at all — and a settled artifact's probe is taken against
    /// `viewport ∩ M_auth` on the strength of `membership ⊆ viewport`, which is then false. The
    /// answer that comes back is about the members in view rather than all of them, which is a
    /// *different question* silently substituted for the one a criterion reads. Equality, and the
    /// view compared too.
    ///
    /// **Every failure is a drop, not an error**, for [`Self::adopt_all`]'s reason: an index that
    /// is not adopted is derived on first use, which is what every request did before the fold
    /// wrote anything.
    pub fn adopt_indexes(
        &self,
        prefix_dir: &std::path::Path,
        prefix: &str,
        extents: &[tessera_store::manifest::TileIndexExtent],
        store: &ArtifactStore,
    ) {
        // Purged for [`Self::adopt_all`]'s reason.
        self.indexes_held
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .retain(|_, (key, _)| key.prefix == prefix);
        for extent in extents {
            let level_version = store.level_version(&extent.layer, extent.level);
            if level_version != extent.level_version {
                tracing::info!(
                    layer = %extent.layer,
                    level = extent.level,
                    view = %extent.view,
                    projected_at = extent.level_version,
                    now = level_version,
                    "a fold-written tile index is not adopted: the level has moved since it was \
                     projected, so its extents are derived on first use"
                );
                continue;
            }
            let path = prefix_dir.join(&extent.path);
            let index = match TileIndex::open(&path) {
                Ok(index) => index,
                Err(error) => {
                    // Loud, because this one is a fault rather than a cadence: the manifest names
                    // a file the prefix should hold and it did not open.
                    tracing::error!(
                        layer = %extent.layer,
                        level = extent.level,
                        view = %extent.view,
                        path = %extent.path,
                        %error,
                        "ALARM: a tile index named by the manifest would not open; candidacy is \
                         correct and the level's index is derived on first use"
                    );
                    continue;
                }
            };
            self.indexes_held
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .insert(
                    (extent.view.clone(), extent.layer.clone(), extent.level),
                    (
                        IndexKey {
                            prefix: prefix.to_string(),
                            level_version,
                        },
                        index,
                    ),
                );
        }
    }

    /// Take the fold-written row-major columns this prefix's manifests name, for every
    /// `(view, layer, level)` whose coordinate still holds.
    ///
    /// **[`Self::adopt_indexes`]' rule, and the direction of a stale one is the same: narrow.** A
    /// growth adds rows the column does not label, and an unlabelled row is one no artifact claims
    /// — so the artifact holding it silently stops being a candidate there, and its masked count
    /// comes back short. Equality on the version, and the view compared too.
    ///
    /// **The manifest's layout tag is checked against the file's own magic** rather than trusted
    /// over it (selection memo §5): a mis-described file refuses at the first bytes, which lands
    /// here as a drop and a recomposition rather than as a misread column.
    pub fn adopt_columns(
        &self,
        prefix_dir: &std::path::Path,
        prefix: &str,
        extents: &[tessera_store::manifest::RowColumnExtent],
        store: &ArtifactStore,
    ) {
        // Purged for [`Self::adopt_all`]'s reason.
        self.columns_held
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .retain(|_, (key, _)| key.prefix == prefix);
        for extent in extents {
            let level_version = store.level_version(&extent.layer, extent.level);
            if level_version != extent.level_version {
                tracing::info!(
                    layer = %extent.layer,
                    level = extent.level,
                    view = %extent.view,
                    written_at = extent.level_version,
                    now = level_version,
                    "a fold-written row-major column is not adopted: the level has moved since it \
                     was written, so it is recomposed on first use"
                );
                continue;
            }
            let path = prefix_dir.join(&extent.path);
            let column = match RowColumn::open(&path, extent.layout) {
                Ok(column) => column,
                Err(error) => {
                    // Loud, because this one is a fault rather than a cadence: the manifest names a
                    // file the prefix should hold, in a form it claims to be in, and it did not
                    // open as that form.
                    tracing::error!(
                        layer = %extent.layer,
                        level = extent.level,
                        view = %extent.view,
                        path = %extent.path,
                        layout = ?extent.layout,
                        %error,
                        "ALARM: a row-major column named by the manifest would not open as the \
                         form the manifest names; the level is recomposed on first use"
                    );
                    continue;
                }
            };
            self.columns_held
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .insert(
                    (extent.view.clone(), extent.layer.clone(), extent.level),
                    (
                        IndexKey {
                            prefix: prefix.to_string(),
                            level_version,
                        },
                        column,
                    ),
                );
        }
    }

    /// The form held for one `(view, layer, level)`, or `None` where none is — see
    /// [`crate::Engine::held_artifact_form_for_test`], its only caller.
    pub fn held_form(&self, view: &str, layer: &str, level: u32) -> Option<Arc<ArtifactRows>> {
        self.cached
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .get(&(view.to_string(), layer.to_string(), level))
            .map(|held| Arc::clone(&held.rows))
    }

    /// File `held` under `address` unless what is there is later on **either** term — the segments
    /// version its rows were brought to, or the level version its records describe (see
    /// [`Held::at`]).
    ///
    /// Both terms are straddles of the same kind: a request builds a form against the generation
    /// and the level version it loaded, and finishes after a flush has extended, a merge has
    /// rebased or a tick has published onto the form it means to replace. Inserted on the tuple
    /// alone, a build that straddled a flush would replace a form five level versions ahead of it
    /// with one at a newer segments version — and since a form behind the store is *served* rather
    /// than rebuilt (`ingest.md` §1.3), the level's masked counts would go backwards for the next
    /// reader. A form that is later on one term and earlier on the other is therefore kept out,
    /// and what it built is discarded; the flush or the tick that follows brings the standing form
    /// forward, and a form neither can bring forward is dropped there and built again.
    fn insert_newest(&self, address: LevelAddress, held: Held) {
        let mut cached = self.cached.lock().unwrap_or_else(|e| e.into_inner());
        if cached.get(&address).is_some_and(|standing| {
            standing.at > held.at || standing.key.level_version > held.key.level_version
        }) {
            return;
        }
        cached.insert(address, held);
    }

    /// How many forms are held. Operator plane only, beside [`Self::builds`] — a count of
    /// structures, naming no artifact and no principal.
    pub fn held(&self) -> usize {
        self.cached.lock().unwrap_or_else(|e| e.into_inner()).len()
    }

    /// Drop everything held for one layer, in every view.
    ///
    /// **Called when the layer is dropped, and this is a retention fix rather than a correctness
    /// one.** A dropped name is tombstoned for ever and the serving path resolves the layer
    /// through the registry before it reaches this cache, so a form left behind could never be
    /// handed to anybody. What it could do is stay: at the campaign's target a level's form is
    /// gigabytes, and nothing here ever removed an entry — the map was bounded by the number of
    /// `(view, layer, level)` triples a process had *ever* seen rather than the number it holds.
    pub fn forget(&self, layer: &str) {
        self.cached
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .retain(|(_, held, _), _| held != layer);
        // **Both maps, or the second one is the retention bug the first one fixed.** A partition is
        // megabytes at the campaign's target and is pinned by nothing else once the layer is gone.
        self.partitions_held
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .retain(|(held, _), _| held != layer);
        // **All three maps, for the same reason.** An unclaimed index is eighty megabytes per level
        // at the campaign's target, pinned by nothing else once the layer is gone.
        self.indexes_held
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .retain(|(_, held, _), _| held != layer);
        // **All four, and this one is the largest of them at 10⁹ rows**: a label column is four
        // bytes a row whatever the artifact count, which is the whole reason the layout exists.
        self.columns_held
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .retain(|(_, held, _), _| held != layer);
        // **All five**, and this one is held rather than claimed, so nothing else would ever remove
        // it: a predicate layer's base column is four bytes a row and is pinned by this map alone.
        self.predicate_bases
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .retain(|(_, held, _), _| held != layer);
    }

    /// Drop everything held for one `(layer, level)`, in every view — **what a layout flip needs**.
    ///
    /// The cached row form is replace-on-mismatch, so a fold's new prefix would replace it at the
    /// level's next request anyway. The *held adoption maps* are the problem the memo names
    /// (selection memo §5): a level that flipped to row-major is never asked for its tile index
    /// again, so nothing ever claims that entry and the last generation's copy is pinned for the
    /// process's life — eighty megabytes per level at the campaign's target, and four bytes a row
    /// for the column in the other direction.
    ///
    /// **Removal only**, so it cannot widen anything: its worst outcome is one level rebuilding
    /// what it would have adopted.
    pub fn forget_level(&self, layer: &str, level: u32) {
        self.cached
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .retain(|(_, held, held_level), _| held != layer || *held_level != level);
        self.partitions_held
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .retain(|(held, held_level), _| held != layer || *held_level != level);
        self.indexes_held
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .retain(|(_, held, held_level), _| held != layer || *held_level != level);
        self.columns_held
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .retain(|(_, held, held_level), _| held != layer || *held_level != level);
        self.predicate_bases
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .retain(|(_, held, held_level), _| held != layer || *held_level != level);
    }

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
        let map_key = (view.to_string(), layer.to_string(), level);
        // **Read under the lock first, and taken out of the map only where there is an amendment
        // to make.** A request arriving while the entry is out finds nothing held and projects the
        // level whole, on the request path — the cost this whole mechanism exists to avoid — so
        // the window is narrowed to the amendment itself (`elapsed_ms` in the line below, 15 ms at
        // rung 3's `mesh/descriptors`) and a level with no delta to take never leaves the map at
        // all.
        //
        // **Taking it is what makes the amendment cheap**: between requests this thread is then
        // the `Arc`'s only holder, so `Arc::make_mut` copies nothing. Cloning the entry instead
        // would leave the map holding a second reference and copy the level's records, generating
        // sets and tile index on every publication.
        let (read_prefix, read_at, pending_from) = {
            let cached = self.cached.lock().unwrap_or_else(|e| e.into_inner());
            let Some(held) = cached.get(&map_key) else {
                return;
            };
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
            return;
        }
        let Some(Held { key, at, mut rows }) = self
            .cached
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .remove(&map_key)
        else {
            // A request replaced or a drop removed the entry between the two locks. Whatever
            // stands there now was filed against the store this delta has already reached.
            return;
        };
        if key.level_version != pending_from || at != read_at {
            // The same race one step in: the entry that came out is not the one that was read, so
            // the run of deltas selected above may not be its own. It goes back untouched and the
            // next tick publishes onto it.
            self.cached
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .insert(map_key, Held { key, at, rows });
            return;
        }
        // **A form that borrowed is rebuilt rather than amended** ([`Self::drop_borrowed`]). The
        // entry is already out of the map, so returning here is what drops it.
        if !rows.inherited.is_empty() {
            drop(rows);
            self.drop_borrowed(&map_key, view);
            return;
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
            return;
        }

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
        // **The rows these deltas gave each artifact**, gathered as the membership takes them, so
        // the column is amended at exactly those and the pack is never rewritten. Empty on an
        // artifact-major level, which has no column to amend.
        let row_major = amended.layout.is_row_major();
        let mut added: Vec<(u32, u32)> = Vec::new();
        // The three arms of `ingest.md` §4.1, counted for the line below: sets unioned into the
        // served operator, operators re-derived whole, and ordinals published.
        let mut unions = 0u64;
        let mut published = 0u64;
        // Ordinals whose records entry is read again — a fill's parts, and the stored cardinality
        // a page moved — and those whose operators are re-derived from entity truth.
        let mut refresh: std::collections::BTreeSet<u32> = std::collections::BTreeSet::new();
        let mut rederive: std::collections::BTreeSet<u32> = std::collections::BTreeSet::new();
        let mut sets_moved = false;
        for delta in &pending {
            match &delta.kind {
                DeltaKind::Grown { joins, pages } => {
                    for (ordinal, joining) in joins {
                        if drawn_record(store, layer, level, *ordinal, view).is_none() {
                            continue;
                        }
                        let fresh = amended.grow_rows(*ordinal, joining, space);
                        unions += 1;
                        if row_major {
                            added.extend(fresh.iter().map(|row| (row, *ordinal)));
                        }
                    }
                    for page in pages {
                        if drawn_record(store, layer, level, page.ordinal, view).is_none() {
                            continue;
                        }
                        sets_moved = true;
                        refresh.insert(page.ordinal);
                        if page.whole {
                            rederive.insert(page.ordinal);
                            continue;
                        }
                        // A join alone: the same set the projection would have produced, reached
                        // by one union over the page's own members. Base rows, as a generating
                        // set's are (`MembershipRows::put`).
                        if amended.grow_generating(page.ordinal, page.rank, &page.joining, space) {
                            unions += 1;
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
                                    published += 1;
                                    let fresh = amended.publish_resolved(
                                        *ordinal,
                                        record,
                                        rows(*ordinal),
                                        space,
                                    );
                                    if row_major {
                                        added.extend(fresh.iter().map(|row| (row, *ordinal)));
                                    }
                                }
                            }
                        }
                    }
                }
                DeltaKind::Published(ordinals) => {
                    for ordinal in ordinals {
                        if let Some(record) = drawn_record(store, layer, level, *ordinal, view) {
                            published += 1;
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
                                added.extend(fresh.iter().map(|row| (row, *ordinal)));
                            }
                        }
                    }
                }
            }
        }
        // **The operator and the cardinality beside it, from one read of the store.** A
        // re-derivation projects the artifact's sets again; a refresh alone takes the declared
        // sizes the pages moved. Both read the record as it stands now, which is what makes the
        // pair a containment test reads a pair one moment produced.
        for ordinal in &refresh {
            if let Some(record) = drawn_record(store, layer, level, *ordinal, view) {
                amended.refresh_sets(*ordinal, record, space, rederive.contains(ordinal));
            }
        }
        let lost = amended.amend_derived(&added, total_rows(space));
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
            unions,
            rederived = rederive.len(),
            published,
            rows_added = added.len(),
            cloned = shared,
            cloned_ms,
            elapsed_ms = started.elapsed().as_millis() as u64,
            "a level's row forms are published from the deltas since the last tick"
        );
        if lost {
            self.fallbacks
                .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
            // [`Self::extend_flushed`]'s arm: a form that holds no bitmaps takes the list form
            // rather than falling back to a membership it does not have. The entry is already out
            // of the map, so a failure here drops it rather than putting it back.
            if !amended.membership().rows_held() {
                let recomposed = std::time::Instant::now();
                let scratch = self.scratch().to_path_buf();
                if !amended.recompose_as_list(&added, &scratch) {
                    self.drop_lost_column(&map_key, view);
                    return;
                }
                self.columns_composed
                    .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
                tracing::warn!(
                    layer = %layer,
                    level,
                    view = %view,
                    elapsed_ms = recomposed.elapsed().as_millis() as u64,
                    "this level's amended memberships no longer partition and it is served from \
                     its column alone, so the column is recomposed in the list form. Every answer \
                     is unchanged; the layout is not"
                );
            } else {
                tracing::warn!(
                    layer = %layer,
                    level,
                    view = %view,
                    "this level's amended memberships no longer partition, so it is served \
                     artifact-major. Every answer is unchanged; the layout is not"
                );
            }
        }
        // **A publication carries the containment partition over; a page of a generating set takes
        // it away.** The partition is composed from the level's records at a version and answers
        // per `(ordinal, rank)`. A membership join changes no set, and a publication only appends
        // ordinals, which `ContainmentAnswers::covers` reports as uncovered and sends to the
        // masked-count route. A page *does* change a set: against a set that has since grown the
        // partition's answer is one about a smaller set, which passes for a principal who does not
        // hold the new member. So the level gives the structure up and serves containment on the
        // masked-count route, which asks `M_auth` itself.
        if sets_moved && amended.partition.is_some() {
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
            .insert(map_key, Held { key, at, rows });
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
                    amended.extend_by_resolved(&piece, extent.row_base)
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
            if lost {
                self.fallbacks
                    .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
                // **A form that holds no bitmaps takes the list form instead of falling back**:
                // its column is its membership, so there is nothing to fall back *to*, and the
                // list form is what a fold would choose for a level that has stopped partitioning
                // (decision 0094). Composed through the disk-backed partition route from the
                // column it already holds — nothing row-sized is held.
                if !rows.membership().rows_held() {
                    let started = std::time::Instant::now();
                    let scratch = self.scratch().to_path_buf();
                    if !Arc::make_mut(&mut rows).recompose_as_list(&added, &scratch) {
                        self.drop_lost_column(&address, view);
                        continue;
                    }
                    self.columns_composed
                        .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
                    tracing::warn!(
                        layer = %layer,
                        level,
                        view = %view,
                        elapsed_ms = started.elapsed().as_millis() as u64,
                        "this level's extended memberships no longer partition and it is served from \
                         its column alone, so the column is recomposed in the list form. Every \
                         answer is unchanged; the layout is not"
                    );
                } else {
                    tracing::warn!(
                        layer = %layer,
                        level,
                        view = %view,
                        "this level's extended memberships no longer partition, so it is served \
                         artifact-major. Every answer is unchanged; the layout is not"
                    );
                }
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
    /// the span's labels and takes the new ones ([`RowColumn::rebase`]); the tile index is derived
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
                SegmentRows::Resolved(piece) => amended.rebase_span_resolved(&piece, next, start),
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
            if lost {
                self.fallbacks
                    .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
                // **A form that holds no bitmaps takes the list form instead of falling back**:
                // its column is its membership, so there is nothing to fall back *to*, and the
                // list form is what a fold would choose for a level that has stopped partitioning
                // (decision 0094). Composed through the disk-backed partition route from the
                // column it already holds — nothing row-sized is held.
                if !rows.membership().rows_held() {
                    let started = std::time::Instant::now();
                    let scratch = self.scratch().to_path_buf();
                    if !Arc::make_mut(&mut rows).recompose_as_list(&added, &scratch) {
                        self.drop_lost_column(&address, view);
                        continue;
                    }
                    self.columns_composed
                        .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
                    tracing::warn!(
                        layer = %layer,
                        level,
                        view = %view,
                        elapsed_ms = started.elapsed().as_millis() as u64,
                        "this level's rebased memberships no longer partition and it is served from \
                         its column alone, so the column is recomposed in the list form. Every \
                         answer is unchanged; the layout is not"
                    );
                } else {
                    tracing::warn!(
                        layer = %layer,
                        level,
                        view = %view,
                        "this level's rebased memberships no longer partition, so it is served \
                         artifact-major. Every answer is unchanged; the layout is not"
                    );
                }
            }
            self.insert_newest(address, Held { key, at, rows });
        }
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

/// Why an artifact is not served. **Every variant produces the same outcome for a caller** —
/// absence — and the distinction exists for logs, tests and the conformance oracle, never for a
/// response body. A route that reported which of these applied would be a disclosure oracle over
/// exactly the facts the predicate exists to withhold.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Withheld {
    /// The level holds no artifact at that ordinal in this view: a hole, an ordinal past the
    /// level's end, or — on a group-scoped layer — an artifact of another view of the group.
    NoArtifact,
    /// The artifact's own entity is deleted or suppressed.
    Verdict,
    /// The viewer may not know the layer exists.
    LayerGate,
    /// The layer declares that its artifacts carry their own terms, and this viewer holds none of
    /// this artifact's.
    OwnTerms,
    /// The masked count does not clear the declared criterion.
    Criterion,
    /// The artifact is an attachment, and what it attaches to is suppressed, deleted, or in a layer
    /// this viewer does not reach. **A label does not outlive the thing it labels.**
    Attachment,
    /// The artifact carries supplied content and this viewer contains no content's generating
    /// set — or the content they would have been served is not readable.
    Containment,
}

/// The outcome of the one predicate.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ArtifactVerdict {
    /// Served, with this masked count beside it, and — where the layer declares supplied content —
    /// the rank of the one content this viewer gets, **entire**.
    Serve {
        masked_count: u64,
        rank: Option<u32>,
    },
    /// Absent. See [`Withheld`] on why the reason never reaches a caller.
    Absent(Withheld),
}

impl ArtifactVerdict {
    pub fn is_served(&self) -> bool {
        matches!(self, ArtifactVerdict::Serve { .. })
    }

    pub fn masked_count(&self) -> Option<u64> {
        match self {
            ArtifactVerdict::Serve { masked_count, .. } => Some(*masked_count),
            ArtifactVerdict::Absent(_) => None,
        }
    }
}

/// Everything the predicate needs about one viewer and one layer, gathered once so the test itself
/// is a straight line.
pub struct ArtifactView<'a, M: MaskedSet> {
    pub declaration: &'a LayerDeclaration,
    pub overlay: &'a Overlay,
    /// The viewer's satisfied terms — the same set the item-visibility predicate uses. Satisfaction
    /// is **intersection** with this set, never a conservative label join: a join yields an empty
    /// required set for a disjunctive gate and admits every principal, which is an error this
    /// codebase has made once already, in the view gate.
    pub satisfied: &'a FxHashSet<TermId>,
    /// Whether the viewer reaches the layer at all. Resolved once per session by the registry, and
    /// passed in rather than recomputed — but see [`ArtifactView::verdict`]: the *overlay* half is
    /// never cached, only this.
    pub layer_reachable: bool,
    /// This view's row form of the layer's membership.
    pub rows: &'a ArtifactRows,
    /// The dependency prerequisite: **is the artifact this one attaches to served to this
    /// viewer?**
    ///
    /// **A hook rather than a resolved answer, because the target is another layer.** The layer
    /// this view is for was resolved once per session; a label's target may live in any layer its
    /// own declares in `depends_on`, and answering for it means that layer's reachability, its live
    /// suppression, its row form and its declaration — a second `verdict`, which the caller is the
    /// one holding the pieces for.
    ///
    /// **Every failure to answer is `false`.** A target layer this viewer does not reach, a layer
    /// dropped since the resolution, a level absent from this view, a slot a fold has emptied, and
    /// a target this viewer is simply not shown are one answer here, for the reason they are one
    /// answer everywhere else: which of them applies is exactly the fact being withheld.
    ///
    /// **Nothing about the target is cached across the call.** A suppression takes effect at the
    /// ack, so its layer's live disposition is asked here in the same order it is asked for the
    /// layer being served — a cached reachability outliving a layer suppression is the fail-open
    /// that ordering exists to avoid.
    pub dependency_served: &'a dyn Fn(&Attachment) -> bool,
    /// The viewer's **composed** mask — see [`MaskedSet`] for why the type forbids anything else.
    pub mask: &'a M,
    /// `deleted ∪ suppressed`, in this view's row space — the generation's own deny mask.
    ///
    /// **The containment partition's acceptance test, and it is not a refinement**
    /// (`design/artifact-serving-at-scale.md` §4.2; the review's finding 2). The partition answers
    /// `G ⊆ M_auth` from term signatures, which a deletion or a suppression does not touch, so an
    /// expression consulted alone is fail-open for exactly the case the write cycle exists to make
    /// safe. This is the same set [`crate::compose`] composed the mask from — re-derived by the
    /// deny lane at the acknowledgement, and on an unsuppress **re-derived rather than
    /// subtracted**, so `delete → suppress → unsuppress` leaves the entity deleted.
    ///
    /// Read only by the partition's arm. The masked-count route needs nothing here: the mask it
    /// counts against already has these rows taken out.
    pub denied: &'a Bitmap,
    /// This principal's answers over the level's containment partition, where the level has one.
    ///
    /// **A fast arm, never a second rule.** `None` puts every artifact on the masked-count route,
    /// which is what a foreign plugin gets and what the probe measures; `Some` answers the same
    /// question from terms and asks the mask only for the deny correction
    /// ([`crate::containment`]). The two must agree rank for rank, and
    /// `tests/artifact_containment.rs` is where that is asserted rather than assumed.
    pub containment: Option<ContainmentAnswers<'a>>,
    /// This session's masked counts over the level, where the level is served row-major —
    /// [decision 0093](../../../docs/decisions/0093-nothing-is-materialised-per-token-over-the-artifact-population.md)'s
    /// one named exception, held per `(session, layer)` and byte-budgeted (`crate::histogram`).
    ///
    /// **`None` on an artifact-major level, and that is not a fallback**: such a level answers
    /// `|membership ∩ M_auth|` one artifact at a time, so a request's budget bounds the work and
    /// there is nothing for a per-session structure to buy.
    ///
    /// **`Some` wherever the level has a column**, which is every level served row-major:
    /// [`crate::Engine::masked_counts`] builds one from the column and nothing else decides. A
    /// column-only form has no per-artifact membership to fall back to, so that is the whole of
    /// the row-major route to this number rather than the fast half of it.
    pub counts: Option<Arc<MaskedCounts>>,
}

impl<M: MaskedSet> ArtifactView<'_, M> {
    /// The one predicate.
    ///
    /// `own_terms` is the artifact's own access label resolved to a term, or `None` if it carries
    /// none. **A layer whose `artifact_visibility` names a field, holding an artifact that carries
    /// no term, withholds it** rather than admitting it: naming the field says the artifact's
    /// existence is gated on its own label, and an artifact with no label has nothing for a viewer
    /// to satisfy. Admitting it would make a missing declaration a grant to everyone, which is the
    /// direction a mistake must never take.
    pub fn verdict(
        &self,
        artifact_entity: EntityId,
        ordinal: u32,
        own_terms: Option<TermId>,
    ) -> ArtifactVerdict {
        // 0. There is an artifact here, in this view. The form was built from this view's slice of
        //    the level ([`ArtifactRows::holds`]), so a hole, an ordinal past the level's end and a
        //    group-scoped artifact belonging to another view of the group are one answer. The
        //    ordinal's address is the caller's — a level's runs map an ordinal to an entity for the
        //    whole layer, not per view — so this is the conjunct that makes the address a fact
        //    about *this* view.
        if !self.rows.holds(ordinal) {
            return ArtifactVerdict::Absent(Withheld::NoArtifact);
        }

        // 1. The overlay, first and unconditional — the same composition a point goes through.
        //    Asked live on every call, never cached beside the reachability above it: a suppression
        //    takes effect at the ack, and a cache that baked in this answer would keep serving a
        //    hidden artifact for the life of a session.
        if self.overlay.is_deleted(artifact_entity) || self.overlay.is_suppressed(artifact_entity) {
            return ArtifactVerdict::Absent(Withheld::Verdict);
        }

        // 2. The layer's gate.
        if !self.layer_reachable {
            return ArtifactVerdict::Absent(Withheld::LayerGate);
        }

        // 3. What it depends on, if it depends on anything: served to this viewer, or this
        //    artifact is absent (decision 0089, rule 2). Running it here is what puts it on every
        //    route — search, a held identifier and a filter reach a label directly and traverse no
        //    edge, so a rule stated only for traversal never reaches them — and running it *before*
        //    the artifact's own terms and its own criterion is what makes it a prerequisite rather
        //    than one conjunct among several: an artifact whose dependency is invisible is absent
        //    without its own membership being touched at all.
        if let Some(attachment) = self.rows.attachment(ordinal) {
            if !(self.dependency_served)(attachment) {
                return ArtifactVerdict::Absent(Withheld::Attachment);
            }
        }

        // 4. The artifact's own terms, if its layer says it carries them.
        if self.declaration.artifact_visibility.carries_own_labels() {
            match own_terms {
                Some(term) if self.satisfied.contains(&term) => {}
                _ => return ArtifactVerdict::Absent(Withheld::OwnTerms),
            }
        }

        // 5. The existence criterion, against the **live** masked count. The same number is
        //    returned to the caller, so the tested quantity and the served quantity cannot drift.
        let masked_count = self.masked_count(ordinal);
        if let Some(criterion) = self.declaration.require_member_visibility {
            let clears = match criterion {
                ExistenceCriterion::Count(n) => masked_count >= n,
                // The declared, unmasked size is the denominator — a predicate input the build
                // computes and the test consumes, with no field and no wire shape carrying it (C8).
                // A zero denominator cannot clear a positive fraction, and saying so explicitly
                // avoids a division nobody wants to reason about.
                ExistenceCriterion::Fraction(p) => {
                    let declared = self.declared_size(ordinal);
                    declared > 0 && (masked_count as f64) >= p * (declared as f64)
                }
            };
            if !clears {
                return ArtifactVerdict::Absent(Withheld::Criterion);
            }
        }

        // 6. Containment, last: the first content whose generating set this viewer holds
        //    **entirely**. A viewer satisfying none receives no artifact — not the artifact with
        //    its description missing, which is the in-between state decision 0076 forbids.
        let rank = match self.containment(ordinal) {
            Containment::NothingToContain => None,
            Containment::Satisfied(i) => Some(i),
            Containment::Unsatisfied => return ArtifactVerdict::Absent(Withheld::Containment),
        };

        ArtifactVerdict::Serve { masked_count, rank }
    }

    /// Containment, by the partition where there is one and by the mask where there is not.
    ///
    /// **One call site, so the two arms cannot be reached by different routes.** Which arm answers
    /// is a property of the level and of the bundle's plugin; it is never a property of the
    /// viewer, and no caller chooses.
    fn containment(&self, ordinal: u32) -> Containment {
        let declares_content = !self.declaration.content.supplied.is_empty();
        if let Some(answers) = &self.containment {
            if let Some(containment) =
                self.rows
                    .satisfied_rank_via(ordinal, answers, self.denied, declares_content)
            {
                return containment;
            }
        }
        self.rows
            .satisfied_rank(ordinal, self.mask, declares_content)
    }

    /// The masked count, from whichever structure this level's layout puts it in.
    ///
    /// **One quantity either way, and it is the number served as well as the number tested.** An
    /// artifact-major level intersects the artifact's own membership with the composed mask; a
    /// row-major level has no per-artifact membership to intersect, and takes the count from the
    /// per-`(session, layer)` histogram — the same walk of the same mask, done once for the level
    /// rather than once per artifact. `tests/artifact_row_major.rs` asserts they agree.
    fn masked_count(&self, ordinal: u32) -> u64 {
        if self.rows.layout().is_row_major() {
            if let Some(counts) = &self.counts {
                return counts.get(ordinal);
            }
        }
        self.rows.masked_count(ordinal, self.mask)
    }

    /// The artifact's full membership size, in **row** terms.
    ///
    /// The proportional criterion's denominator, and the one place it is read. Taken from the row
    /// form rather than the entity form so that numerator and denominator come from the same
    /// projection: a member whose row still sits in an unfolded flush extent contributes to
    /// neither, which understates the ratio — fail-closed, and the same posture the write cycle
    /// takes for an unrebuilt member.
    ///
    /// **On a row-major level it comes from the column instead**, which is §10's answer to the same
    /// question in the same row space: an artifact's declared size is how many rows carry its
    /// label. The two are equal by construction — both count the artifact's projected rows — which
    /// is what lets the criterion behave identically under either layout.
    fn declared_size(&self, ordinal: u32) -> u64 {
        if let Some(column) = self.rows.column() {
            return column.declared_size(ordinal);
        }
        self.rows.get(ordinal).map(Bitmap::cardinality).unwrap_or(0)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tessera_lifecycle::wal::ChangeOp;
    use tessera_types::layer::{
        ArtifactVisibility, ContentDeclaration, Hierarchy, HierarchyKind, MembershipSource,
    };

    fn declaration(
        carries_own_labels: bool,
        criterion: Option<ExistenceCriterion>,
    ) -> LayerDeclaration {
        LayerDeclaration {
            scope: Default::default(),
            name: "clusters/a".into(),
            title: Some("A".into()),
            views: vec!["s0".into()],
            membership: MembershipSource::Enumerated,
            value_set: Default::default(),
            visibility: None,
            artifact_visibility: if carries_own_labels {
                ArtifactVisibility::carried("visibility")
            } else {
                ArtifactVisibility::inherited()
            },
            require_member_visibility: criterion,
            hierarchy: Hierarchy {
                kind: HierarchyKind::Flat,
                prune_children: false,
            },
            content: ContentDeclaration::default(),
            depends_on: Vec::new(),
            levels: Vec::new(),
            layout: None,
            shape: None,
        }
    }

    /// Row-space memberships without a `RowSpace` to project through — these tests are about the
    /// predicate, and building a permutation would test the projection instead.
    fn rows_of(sets: &[&[u32]]) -> ArtifactRows {
        assembled(
            ArtifactRecords {
                attachments: vec![None; sets.len()],
                parents: vec![Vec::new(); sets.len()],
                declared: vec![Vec::new(); sets.len()],
            },
            MembershipRows {
                rows: sets.iter().map(|s| Some(Arc::new(Bitmap::of(s)))).collect(),
                generating: vec![Vec::new(); sets.len()],
                rows_held: true,
            },
        )
    }

    /// The two halves plus the index derived over them — the shape [`ArtifactRows::build`] produces
    /// from a store, assembled by hand for the cases that are about the predicate.
    fn assembled(records: ArtifactRecords, membership: MembershipRows) -> ArtifactRows {
        let index = TileIndex::build(&membership, 0);
        ArtifactRows {
            records,
            membership,
            index,
            partition: None,
            layout: ServingLayout::ArtifactMajor,
            column: None,
            base_rows: 0,
            covered: Vec::new(),
            inherited: Vec::new(),
        }
    }

    /// The prerequisite where the dependency is served — the ordinary state, so that a case about
    /// some other conjunct is not silently answered by this one instead.
    fn dependency_served(_attachment: &Attachment) -> bool {
        true
    }

    /// The prerequisite where the dependency is not served to this viewer: suppressed, deleted,
    /// gone at a fold, in a layer they do not reach, or simply below its own layer's bar. The
    /// predicate treats them alike, and which of them applies is exactly what is withheld.
    fn dependency_absent(_attachment: &Attachment) -> bool {
        false
    }

    /// One artifact, with ranked contents given as `(generating set, declared size)` — the
    /// declared size separate so a test can build the *lossy projection* case, where row space
    /// holds fewer members than the entity-space set the caller published.
    fn rows_with_contents(members: &[u32], contents: &[(&[u32], u64)]) -> ArtifactRows {
        assembled(
            ArtifactRecords {
                attachments: vec![None],
                parents: vec![Vec::new()],
                declared: vec![contents.iter().map(|(_, declared)| *declared).collect()],
            },
            MembershipRows {
                rows: vec![Some(Arc::new(Bitmap::of(members)))],
                generating: vec![contents.iter().map(|(set, _)| Bitmap::of(set)).collect()],
                rows_held: true,
            },
        )
    }

    struct Fixture {
        overlay: Overlay,
        satisfied: FxHashSet<TermId>,
        rows: ArtifactRows,
        mask: Bitmap,
        denied: Bitmap,
    }

    impl Fixture {
        fn new(members: &[&[u32]], mask: &[u32]) -> Self {
            Fixture {
                overlay: Overlay::new(),
                satisfied: FxHashSet::default(),
                rows: rows_of(members),
                mask: Bitmap::of(mask),
                denied: Bitmap::new(),
            }
        }

        fn view<'a>(
            &'a self,
            declaration: &'a LayerDeclaration,
            reachable: bool,
        ) -> ArtifactView<'a, Bitmap> {
            ArtifactView {
                declaration,
                overlay: &self.overlay,
                satisfied: &self.satisfied,
                layer_reachable: reachable,
                rows: &self.rows,
                mask: &self.mask,
                dependency_served: &dependency_served,
                containment: None,
                denied: &self.denied,
                counts: None,
            }
        }
    }

    /// **The stage's headline.** Two principals, one artifact, two different counts — and neither
    /// is the artifact's size. A count equal to the membership would mean the mask was never
    /// applied, which is the failure that looks most like success.
    #[test]
    fn the_count_is_the_viewers_own_and_never_the_artifacts_size() {
        let d = declaration(false, None);
        let members: &[&[u32]] = &[&[1, 2, 3, 4, 5, 6, 7, 8, 9, 10]];

        let broad = Fixture::new(members, &[1, 2, 3, 4, 5, 6, 7, 8]);
        let narrow = Fixture::new(members, &[9, 10, 11, 12]);

        let broad_count = broad
            .view(&d, true)
            .verdict(EntityId::new(999), 0, None)
            .masked_count()
            .unwrap();
        let narrow_count = narrow
            .view(&d, true)
            .verdict(EntityId::new(999), 0, None)
            .masked_count()
            .unwrap();

        assert_eq!(broad_count, 8);
        assert_eq!(narrow_count, 2);
        assert_ne!(broad_count, 10, "the declared size must never be served");
        assert_ne!(narrow_count, 10);
    }

    /// The criterion decides existence and never modifies a number: an artifact that clears it is
    /// served with the *same* count that was tested.
    #[test]
    fn the_criterion_decides_existence_and_leaves_the_count_alone() {
        let d = declaration(false, Some(ExistenceCriterion::Count(5)));
        let fx = Fixture::new(&[&[1, 2, 3, 4, 5, 6, 7, 8, 9, 10]], &[1, 2, 3, 4, 5, 6]);
        assert_eq!(
            fx.view(&d, true).verdict(EntityId::new(999), 0, None),
            ArtifactVerdict::Serve {
                masked_count: 6,
                rank: None
            }
        );

        // One fewer visible member and the artifact is absent — not served with a rounded count,
        // not refused, absent.
        let below = Fixture::new(&[&[1, 2, 3, 4, 5, 6, 7, 8, 9, 10]], &[1, 2, 3, 4]);
        assert_eq!(
            below.view(&d, true).verdict(EntityId::new(999), 0, None),
            ArtifactVerdict::Absent(Withheld::Criterion)
        );
    }

    /// The proportional form divides by the declared size, so the same masked count can pass on a
    /// small artifact and fail on a large one. That is the whole reason it exists — a fixed bar of
    /// fifty protects a cluster of a hundred and does nothing for a cluster of ten thousand.
    #[test]
    fn the_proportional_criterion_scales_where_the_absolute_one_does_not() {
        let d = declaration(false, Some(ExistenceCriterion::Fraction(0.5)));

        let small: Vec<u32> = (0..10).collect();
        let large: Vec<u32> = (0..1000).collect();
        let mask: Vec<u32> = (0..6).collect();

        let fx = Fixture::new(&[&small, &large], &mask);
        let view = fx.view(&d, true);
        // 6 of 10 visible: clears 50%.
        assert!(view.verdict(EntityId::new(999), 0, None).is_served());
        // 6 of 1000 visible: does not.
        assert_eq!(
            view.verdict(EntityId::new(998), 1, None),
            ArtifactVerdict::Absent(Withheld::Criterion)
        );
    }

    /// The overlay is first and unconditional. A suppressed artifact is absent even when every
    /// other conjunct passes — and it is asked live, so no cached reachability can outlive it.
    #[test]
    fn a_suppression_beats_every_other_conjunct() {
        let d = declaration(false, None);
        let mut fx = Fixture::new(&[&[1, 2, 3]], &[1, 2, 3]);
        let entity = EntityId::new(999);
        assert!(fx.view(&d, true).verdict(entity, 0, None).is_served());

        fx.overlay.apply(entity, ChangeOp::Suppress);
        assert_eq!(
            fx.view(&d, true).verdict(entity, 0, None),
            ArtifactVerdict::Absent(Withheld::Verdict)
        );

        // And a deletion, which is the irreversible one.
        let mut fx = Fixture::new(&[&[1, 2, 3]], &[1, 2, 3]);
        fx.overlay.apply(entity, ChangeOp::Delete);
        assert_eq!(
            fx.view(&d, true).verdict(entity, 0, None),
            ArtifactVerdict::Absent(Withheld::Verdict)
        );
    }

    /// The own-terms flag and the criterion are **independent** declarations composed by
    /// conjunction. This is decision 0079's whole point: under the three modes it replaced,
    /// declaring a layer *substitutive* switched the criterion off.
    #[test]
    fn the_own_terms_flag_does_not_disable_the_criterion() {
        let d = declaration(true, Some(ExistenceCriterion::Count(5)));
        let mut fx = Fixture::new(&[&[1, 2, 3, 4, 5, 6, 7, 8, 9, 10]], &[1, 2, 3, 4]);
        fx.satisfied.insert(TermId::new(7));

        // Terms satisfied, criterion not: still absent. Under the old gate modes this artifact
        // would have been served, with an exact masked count of 4.
        assert_eq!(
            fx.view(&d, true)
                .verdict(EntityId::new(999), 0, Some(TermId::new(7))),
            ArtifactVerdict::Absent(Withheld::Criterion)
        );
    }

    /// A layer declaring that its artifacts carry their own terms, and an artifact carrying none,
    /// is withheld. Admitting it would make a missing declaration a grant to everyone.
    #[test]
    fn an_artifact_with_no_terms_on_a_layer_carrying_own_labels_is_withheld() {
        let d = declaration(true, None);
        let mut fx = Fixture::new(&[&[1, 2, 3]], &[1, 2, 3]);
        fx.satisfied.insert(TermId::new(7));
        let view = fx.view(&d, true);

        assert_eq!(
            view.verdict(EntityId::new(999), 0, None),
            ArtifactVerdict::Absent(Withheld::OwnTerms)
        );
        assert_eq!(
            view.verdict(EntityId::new(999), 0, Some(TermId::new(8))),
            ArtifactVerdict::Absent(Withheld::OwnTerms),
            "a term the viewer does not hold is no better than none"
        );
        assert!(view
            .verdict(EntityId::new(999), 0, Some(TermId::new(7)))
            .is_served());

        // And on a layer that does *not* declare the flag, a carried term is simply not consulted:
        // the artifact's existence derives from its members' visibility instead.
        let derived = declaration(false, None);
        assert!(fx
            .view(&derived, true)
            .verdict(EntityId::new(999), 0, Some(TermId::new(8)))
            .is_served());
    }

    /// An unreachable layer withholds every artifact in it, before any membership is touched.
    #[test]
    fn an_unreachable_layer_withholds_its_artifacts() {
        let d = declaration(false, None);
        let fx = Fixture::new(&[&[1, 2, 3]], &[1, 2, 3]);
        assert_eq!(
            fx.view(&d, false).verdict(EntityId::new(999), 0, None),
            ArtifactVerdict::Absent(Withheld::LayerGate)
        );
    }

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

    // ---- containment ------------------------------------------------------------------------

    /// **Containment is all or nothing, and it is not a coverage fraction.** A viewer holding every
    /// member of the generating set but one is served nothing — not the artifact with its
    /// description missing, and not a partial description.
    #[test]
    fn a_viewer_missing_one_member_of_the_generating_set_is_served_nothing() {
        let d = declaration(false, None);
        let generating: &[u32] = &[10, 11, 12, 13];
        let rows = rows_with_contents(&[1, 2, 3, 10, 11, 12, 13], &[(generating, 4)]);

        // Holds all four: served, and told which content.
        let all = Bitmap::of(&[1, 2, 3, 10, 11, 12, 13]);
        let view = ArtifactView {
            declaration: &d,
            overlay: &Overlay::new(),
            satisfied: &FxHashSet::default(),
            layer_reachable: true,
            rows: &rows,
            mask: &all,
            dependency_served: &dependency_served,
            containment: None,
            denied: &Bitmap::new(),
            counts: None,
        };
        assert_eq!(
            view.verdict(EntityId::new(999), 0, None),
            ArtifactVerdict::Serve {
                masked_count: 7,
                rank: Some(0)
            }
        );

        // Holds three of the four — and a *larger* visible set overall than a viewer who would
        // pass, which is the point: what decides is which documents, never how many.
        let nearly = Bitmap::of(&[1, 2, 3, 4, 5, 6, 7, 8, 9, 10, 11, 12]);
        let view = ArtifactView {
            declaration: &d,
            overlay: &Overlay::new(),
            satisfied: &FxHashSet::default(),
            layer_reachable: true,
            rows: &rows,
            mask: &nearly,
            dependency_served: &dependency_served,
            containment: None,
            denied: &Bitmap::new(),
            counts: None,
        };
        assert_eq!(
            view.verdict(EntityId::new(999), 0, None),
            ArtifactVerdict::Absent(Withheld::Containment)
        );
    }

    /// **The ranking is the caller's and the service takes no opinion on it** (decision 0078): the
    /// first content the viewer contains is the one they get, entire. This is the worked example
    /// the design turns on — a broad viewer and a narrow viewer failing the *same* full-sample
    /// label, and both satisfying a narrower one.
    #[test]
    fn the_first_content_the_viewer_contains_is_the_one_they_get() {
        let d = declaration(false, None);
        // Rank 0 was generated from the whole sample; rank 1 from a single term's worth.
        let rows = rows_with_contents(
            &[1, 2, 3, 4, 5, 6],
            &[(&[1, 2, 3, 4, 5, 6], 6), (&[1, 2], 2)],
        );
        let verdict = |mask: &Bitmap| {
            ArtifactView {
                declaration: &d,
                overlay: &Overlay::new(),
                satisfied: &FxHashSet::default(),
                layer_reachable: true,
                rows: &rows,
                mask,
                dependency_served: &dependency_served,
                containment: None,
                denied: &Bitmap::new(),
                counts: None,
            }
            .verdict(EntityId::new(999), 0, None)
        };

        // Two viewers with nothing in common beyond the narrow set, and neither holds the whole
        // sample: both fail rank 0 and both are served rank 1.
        for mask in [Bitmap::of(&[1, 2, 3, 4]), Bitmap::of(&[1, 2, 5, 6])] {
            match verdict(&mask) {
                ArtifactVerdict::Serve { rank, .. } => assert_eq!(rank, Some(1)),
                other => panic!("expected the narrow content, got {other:?}"),
            }
        }
        // And a viewer holding everything gets the caller's first choice rather than the fallback.
        match verdict(&Bitmap::of(&[1, 2, 3, 4, 5, 6])) {
            ArtifactVerdict::Serve { rank, .. } => assert_eq!(rank, Some(0)),
            other => panic!("expected the ranked-first content, got {other:?}"),
        }
        // A viewer holding none of it receives no artifact — not the count without the label.
        assert_eq!(
            verdict(&Bitmap::of(&[3, 4])),
            ArtifactVerdict::Absent(Withheld::Containment)
        );
    }

    /// **A generating set that lost members in projection fails for everybody.**
    ///
    /// Row space holds what this view has folded in; a member awaiting a fold projects to nothing.
    /// Testing the projected set alone would let a viewer be contained in a *smaller* set than the
    /// caller declared — containment passing on a set the caller never wrote.
    #[test]
    fn a_generating_set_that_did_not_survive_projection_contains_nobody() {
        let d = declaration(false, None);
        // Two rows survived; the caller published four.
        let rows = rows_with_contents(&[1, 2, 3], &[(&[1, 2], 4)]);
        let everything = Bitmap::from_range(0..1000);
        let view = ArtifactView {
            declaration: &d,
            overlay: &Overlay::new(),
            satisfied: &FxHashSet::default(),
            layer_reachable: true,
            rows: &rows,
            mask: &everything,
            dependency_served: &dependency_served,
            containment: None,
            denied: &Bitmap::new(),
            counts: None,
        };
        assert_eq!(
            view.verdict(EntityId::new(999), 0, None),
            ArtifactVerdict::Absent(Withheld::Containment),
            "a viewer who can see every row there is must still not be served a set that lost \
             members on the way into row space"
        );
    }

    // ---- the dependency prerequisite --------------------------------------------------------

    /// The cluster a label hangs from, as these tests address it.
    const CLUSTERS: &str = "clusters/a";
    const CLUSTER_ENTITY: EntityId = EntityId::new(4_294_901_760);
    const LABEL_ENTITY: EntityId = EntityId::new(4_294_836_224);

    /// One label, attached to a cluster in another layer.
    fn attached_rows(members: &[u32]) -> ArtifactRows {
        assembled(
            ArtifactRecords {
                attachments: vec![Some(Attachment {
                    layer: CLUSTERS.to_string(),
                    level: 0,
                    ordinal: 3,
                    entity: CLUSTER_ENTITY,
                })],
                parents: vec![Vec::new()],
                declared: vec![Vec::new()],
            },
            MembershipRows {
                rows: vec![Some(Arc::new(Bitmap::of(members)))],
                generating: vec![Vec::new()],
                rows_held: true,
            },
        )
    }

    /// **The whole of rule 2 at this level**: a label whose cluster is not served to this viewer is
    /// absent, with every other conjunct passing — its own layer reachable, its own entity
    /// untouched, its whole membership visible and no criterion to fail. Whether the cluster was
    /// suppressed, deleted, folded away, gated or simply below its own bar is the caller's
    /// business, and every one of those answers arrives here as the same `false`.
    #[test]
    fn a_label_whose_dependency_is_not_served_is_absent() {
        let d = declaration(false, None);
        let rows = attached_rows(&[1, 2, 3]);
        let mask = Bitmap::of(&[1, 2, 3]);
        let overlay = Overlay::new();
        let verdict = |prerequisite: &dyn Fn(&Attachment) -> bool| {
            ArtifactView {
                declaration: &d,
                overlay: &overlay,
                satisfied: &FxHashSet::default(),
                layer_reachable: true,
                rows: &rows,
                mask: &mask,
                dependency_served: prerequisite,
                containment: None,
                denied: &Bitmap::new(),
                counts: None,
            }
            .verdict(LABEL_ENTITY, 0, None)
        };

        assert!(
            verdict(&dependency_served).is_served(),
            "the same label with its cluster served — so what the case below asserts is the \
             prerequisite and nothing else"
        );
        assert_eq!(
            verdict(&dependency_absent),
            ArtifactVerdict::Absent(Withheld::Attachment)
        );
    }

    /// **A suppression of the label's own entity still beats everything**, prerequisite included:
    /// the overlay is branch 1 and the dependency term is branch 3, so a served cluster cannot
    /// rescue a suppressed label.
    #[test]
    fn a_suppressed_label_stays_absent_with_its_dependency_served() {
        let d = declaration(false, None);
        let rows = attached_rows(&[1, 2, 3]);
        let mask = Bitmap::of(&[1, 2, 3]);
        let mut overlay = Overlay::new();
        overlay.apply(LABEL_ENTITY, ChangeOp::Suppress);
        assert_eq!(
            ArtifactView {
                declaration: &d,
                overlay: &overlay,
                satisfied: &FxHashSet::default(),
                layer_reachable: true,
                rows: &rows,
                mask: &mask,
                dependency_served: &dependency_served,
                containment: None,
                denied: &Bitmap::new(),
                counts: None,
            }
            .verdict(LABEL_ENTITY, 0, None),
            ArtifactVerdict::Absent(Withheld::Verdict)
        );
    }

    /// **The prerequisite is asked before the artifact's own membership is looked at**, which is
    /// what makes it a prerequisite: a label whose cluster is invisible is absent for *that*
    /// reason, not because it also happened to fail its own criterion.
    #[test]
    fn the_prerequisite_precedes_the_labels_own_criterion() {
        let d = declaration(false, Some(ExistenceCriterion::Count(50)));
        let rows = attached_rows(&[1, 2, 3]);
        let mask = Bitmap::of(&[1, 2, 3]);
        assert_eq!(
            ArtifactView {
                declaration: &d,
                overlay: &Overlay::new(),
                satisfied: &FxHashSet::default(),
                layer_reachable: true,
                rows: &rows,
                mask: &mask,
                dependency_served: &dependency_absent,
                containment: None,
                denied: &Bitmap::new(),
                counts: None,
            }
            .verdict(LABEL_ENTITY, 0, None),
            ArtifactVerdict::Absent(Withheld::Attachment)
        );
    }

    /// An artifact that depends on nothing asks nothing, so a clustering pays no second verdict
    /// per cluster for a relationship it does not have.
    #[test]
    fn an_unattached_artifact_never_consults_the_prerequisite() {
        let d = declaration(false, None);
        let rows = rows_of(&[&[1, 2, 3]]);
        let mask = Bitmap::of(&[1, 2, 3]);
        // A prerequisite that panics rather than one that refuses: refusing would let this pass
        // for a predicate that asked and was told no, which is a different property.
        let never =
            |_: &Attachment| -> bool { panic!("an unattached artifact asked its dependency") };
        assert!(ArtifactView {
            declaration: &d,
            overlay: &Overlay::new(),
            satisfied: &FxHashSet::default(),
            layer_reachable: true,
            rows: &rows,
            mask: &mask,
            dependency_served: &never,
            containment: None,
            denied: &Bitmap::new(),
            counts: None,
        }
        .verdict(EntityId::new(999), 0, None)
        .is_served());
    }

    /// A layer declaring no supplied content has nothing to contain, and its artifacts serve on the
    /// other conjuncts alone — which is every artifact Stage 2 could publish.
    #[test]
    fn an_artifact_with_no_contents_has_nothing_to_contain() {
        let d = declaration(false, None);
        let fx = Fixture::new(&[&[1, 2, 3]], &[1, 2, 3]);
        assert_eq!(
            fx.view(&d, true).verdict(EntityId::new(999), 0, None),
            ArtifactVerdict::Serve {
                masked_count: 3,
                rank: None
            }
        );
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
