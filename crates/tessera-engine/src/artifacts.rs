//! Whether an artifact is served, and what number sits beside it.
//!
//! **One predicate, evaluated on every route.** The viewport, drill-down, filters, search, edge
//! traversal and metadata all call [`ArtifactView::verdict`] and nothing else. Where a route cannot
//! afford it, the route does not exist — that is what keeps the leak register exhaustive by
//! construction rather than by audit.
//!
//! The order of the conjuncts is not cosmetic:
//!
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
    /// Per ordinal, this artifact's parent edge as the registry holds it — read by the serving
    /// path to name a parent that is *also* in the response, and by nothing in [`ArtifactView`].
    /// It is not a visibility term: see [`ArtifactRecord::parent`].
    parents: Vec<Option<ParentRef>>,
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
#[derive(Debug, Clone, Default)]
pub struct MembershipRows {
    /// Parallel to a level's ordinals; `None` is a hole, not an empty membership.
    rows: Vec<Option<Bitmap>>,
    /// Per ordinal, per rank: that content's **generating set** in row space. Pushed in lockstep
    /// with [`ArtifactRecords::declared`], which is the size the same set had in entity space.
    generating: Vec<Vec<Bitmap>>,
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
    /// **A spatial level's membership**, where this level has one: the declared shapes decomposed
    /// into row ranges against this generation's segments (`crate::ranges`).
    ///
    /// `None` on every other layout, and its presence is what makes a level's answers come from the
    /// ranges rather than from the row form — which for such a level holds nothing, its artifacts
    /// carrying a box instead of a stored membership.
    ranges: Option<Arc<crate::ranges::RangeSets>>,
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
    ///
    /// **A spatial level's ranges arrive here too**, and for the identical reason: `candidates`
    /// asks each range whether `viewport ∩ M_auth` holds anything in it, so an ordinal that comes
    /// back has a visible member in view by construction. What differs is only what was walked —
    /// labels per row against ranges per artifact — never the question or the answer.
    Scanned(Bitmap),
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
            self.parents.resize_with(idx + 1, || None);
            self.declared.resize_with(idx + 1, Vec::new);
        }
        self.attachments[idx] = record.attached_to.clone();
        self.parents[idx] = record.parent;
        self.declared[idx] = record
            .contents
            .iter()
            .map(|v| v.generated_from.cardinality())
            .collect();
    }

    /// What the artifact at `ordinal` hangs from, if it hangs from anything.
    pub(crate) fn attachment(&self, ordinal: u32) -> Option<&Attachment> {
        self.attachments
            .get(ordinal as usize)
            .and_then(Option::as_ref)
    }

    /// The artifact's parent edge, as the registry holds it.
    pub(crate) fn parent(&self, ordinal: u32) -> Option<ParentRef> {
        self.parents.get(ordinal as usize).copied().flatten()
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

    fn put(&mut self, idx: usize, record: &ArtifactRecord, space: &RowSpace) {
        if self.rows.len() <= idx {
            self.rows.resize_with(idx + 1, || None);
            self.generating.resize_with(idx + 1, Vec::new);
        }
        self.rows[idx] = Some(space.project_base(&record.members));
        self.generating[idx] = record
            .contents
            .iter()
            // Base rows here too, and here the consequence is sharper than a low count: a
            // generating set that lost members in projection can never be contained, so a label
            // whose sample includes documents ingested since the last fold is withheld from
            // **everyone** until that fold. Fail-closed, and the direction this must fail in — the
            // alternative is serving content on a set that no longer names what the text was
            // derived from.
            .map(|v| space.project_base(&v.generated_from))
            .collect();
    }

    pub fn get(&self, ordinal: u32) -> Option<&Bitmap> {
        self.rows.get(ordinal as usize).and_then(Option::as_ref)
    }

    /// A row form given directly — **test-only**, so that no release build can put a membership
    /// where a projection belongs. [`crate::tile_index`]'s own cases are about the hierarchy over a
    /// row form, and projecting through a permutation would test the projection instead.
    #[cfg(test)]
    pub(crate) fn of_rows(rows: Vec<Option<Bitmap>>) -> Self {
        MembershipRows {
            generating: vec![Vec::new(); rows.len()],
            rows,
        }
    }

    /// The projected generating sets, per rank. Parallel to [`ArtifactRecords::declared`].
    fn generating(&self, ordinal: u32) -> &[Bitmap] {
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
    /// is derived from it ([`tessera_store::membership::observe_shape`]).
    pub fn shape(&self, row_count: u32) -> crate::layout::LevelShape {
        tessera_store::membership::observe_shape(row_count, &|visit| {
            for ordinal in 0..self.len() as u32 {
                if let Some(rows) = self.get(ordinal) {
                    visit(ordinal, rows);
                }
            }
        })
    }
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
        let index = match adopted {
            Some(index) if index.len() == membership.len() => index,
            Some(index) => {
                tracing::warn!(
                    adopted_ordinals = index.len(),
                    level_ordinals = membership.len(),
                    "a fold-written tile index covers a different ordinal range from the level it \
                     was offered for; it is dropped and the level's index is derived"
                );
                TileIndex::build(&membership, space.base_rows())
            }
            None => TileIndex::build(&membership, space.base_rows()),
        };
        ArtifactRows {
            records,
            membership,
            index,
            partition: None,
            layout: ServingLayout::ArtifactMajor,
            column: None,
            ranges: None,
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

    /// Serve this level from `ranges` — a spatial level's membership, re-derived per generation.
    ///
    /// **`None` leaves the level on the artifact-major route over an empty row form**, which serves
    /// nothing: a shape layer stores no membership, so a level whose ranges could not be built has
    /// no members anywhere. That is the fail-closed direction and the same one a row-major level's
    /// missing column takes.
    pub fn with_ranges(mut self, ranges: Option<Arc<crate::ranges::RangeSets>>) -> Self {
        self.layout = match &ranges {
            Some(_) => ServingLayout::SpatialRanges,
            None => ServingLayout::ArtifactMajor,
        };
        self.ranges = ranges;
        self
    }

    /// This level's row ranges, where its membership is a shape.
    pub fn ranges(&self) -> Option<&crate::ranges::RangeSets> {
        self.ranges.as_deref()
    }

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
    pub fn candidacy(&self, viewport: &crate::tile_index::Viewport<'_>) -> Candidacy {
        // **Three routes and one question.** The ranges arm and the column arm both answer against
        // `viewport ∩ M_auth` and are therefore exact for the masked question as well; the indexed
        // arm is a candidate generator and every ordinal it returns still pays a probe.
        if let Some(ranges) = &self.ranges {
            return Candidacy::Scanned(ranges.candidates(viewport.here()));
        }
        match &self.column {
            Some(column) => Candidacy::Scanned(column.candidates(viewport.here())),
            None => Candidacy::Indexed(self.index.candidates(viewport.rows())),
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

    /// The artifact's parent edge, as the registry holds it.
    pub(crate) fn parent(&self, ordinal: u32) -> Option<ParentRef> {
        self.records.parent(ordinal)
    }

    pub fn get(&self, ordinal: u32) -> Option<&Bitmap> {
        self.membership.get(ordinal)
    }

    /// How many ordinals this level covers, holes included.
    pub fn len(&self) -> usize {
        self.membership.len()
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
            // and an artifact here with none has had its last content withdrawn by a fold under
            // the strict declaration. Serving it would be the identity and the count with the
            // description missing — the in-between state decision 0076 forbids — so it is absent
            // until the caller republishes.
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
/// **The segments version is deliberately not a term, and that is what the base-row rule buys.**
/// A flush and a merge both move it, and both leave every bit of this projection correct: the form
/// holds base rows only ([`RowSpace::project_base`]), an append adds none of them and a merge
/// renumbers only the extent rows above them. Keying on it instead would rebuild every level on
/// every flush — tens of seconds per level at 10⁷ artifacts, paid by whichever request arrived
/// next, for a set of bits that did not move. The one operation that *does* renumber the base is
/// the fold, and a fold publishes a new prefix.
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
/// **The segments version is deliberately not a term, and that is what the base-row rule buys.**
/// A flush and a merge both move it, and both leave every bit of this projection correct: the form
/// holds base rows only ([`RowSpace::project_base`]), an append adds none of them and a merge
/// renumbers only the extent rows above them. Keying on it instead would rebuild every level on
/// every flush — tens of seconds per level at 10⁷ artifacts, paid by whichever request arrived
/// next, for a set of bits that did not move. The one operation that *does* renumber the base is
/// the fold, and a fold publishes a new prefix.
/// **Where a predicate level's membership comes from**, resolved against this generation — the
/// pieces [`ArtifactProjections::get_or_build`] needs and cannot reach itself.
///
/// **`None` is an enumerated level**, and that is not a fallback: such a level's membership is
/// stored, so there is no rule to evaluate and nothing here to supply.
pub enum PredicateSource<'a> {
    /// `membership = { attribute = f }` — the indexed column `f`, addressed by entity.
    Attribute(AttributeSource<'a>),
    /// `membership = "spatial"` — the declared boxes, and the geometry they are covered against.
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

/// The declared shapes a spatial layer's membership is drawn from, and what they are drawn against.
pub struct SpatialSource<'a> {
    /// The Morton depth the layer declares. **Part of the membership**, not a tuning key.
    pub depth: u8,
    /// The view's extent — the frame the boxes are quantised in. A box quantised against a
    /// different extent covers different tiles, which is why this comes from the view rather than
    /// from the request.
    pub extent: tessera_spatial::Bounds,
    /// This generation's segments and their row bases, base first. **Every segment**, which is what
    /// makes the ranges fresh by construction: a flush publishes one, the next request resolves the
    /// same box against a list that now includes it, and the points in it count.
    pub segments: &'a [(&'a tessera_store::read::SegmentData, u32)],
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct ProjectionKey {
    prefix: String,
    view: String,
    level_version: u64,
    /// **The geometry a *predicate* level's membership was evaluated against, and `0` for every
    /// other level.**
    ///
    /// A stored membership is base-row-addressed and survives a flush, which is what
    /// [`RowSpace::project_base`] buys and why `segments_version` is deliberately not a term above.
    /// A predicate's membership is not stored: an attribute layer's live tail covers the rows a
    /// flush appended, and a shape's ranges are resolved against the segment list itself. Both move
    /// when the geometry does, so both are rebuilt then — which is the whole of *never stale*, and
    /// the cost of it is that a predicate level's form is derived once per flush rather than once
    /// per fold.
    ///
    /// **Zero rather than an `Option`**, because a level either has a rule to evaluate or it does
    /// not: an enumerated level filed under a geometry would rebuild on every flush for a set of
    /// bits that did not move.
    live: u64,
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

#[derive(Debug, Default)]
pub struct ArtifactProjections {
    cached: Mutex<BTreeMap<LevelAddress, (ProjectionKey, Arc<ArtifactRows>)>>,
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
}

impl ArtifactProjections {
    pub fn new() -> Self {
        Self::default()
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

    /// This level's row form for the generation `store` is in, building it if what is held is
    /// stale.
    ///
    /// **The version is read from the store this builds from, never handed in.** Both come from
    /// the one borrow, so the form cached under version *v* is the form of the level *at* version
    /// *v* — there is no window in which a caller's separately-read version could label a form
    /// built from records that have since moved. That is the whole of the freshness argument, and
    /// a stale form here is a wrong masked count with nothing reporting a fault.
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
    ) -> Arc<ArtifactRows> {
        let key = ProjectionKey {
            prefix: prefix.to_string(),
            view: view.to_string(),
            level_version: store.level_version(layer, level),
            // See [`ProjectionKey::live`]: a rule is evaluated against the geometry, a stored
            // membership is not.
            live: match predicate {
                Some(_) => segments_version,
                None => 0,
            },
        };
        let map_key = (view.to_string(), layer.to_string(), level);

        if let Some((held, rows)) = self
            .cached
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .get(&map_key)
        {
            if *held == key {
                return Arc::clone(rows);
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
        let adopted = self.claim_index(prefix, view, layer, level, key.level_version);
        let from_prefix = adopted.is_some();
        let built = ArtifactRows::build_over(store.level(layer, level), space, adopted)
            .with_partition(partition);
        // **The column, claimed from the prefix or composed from the form just built** — and the
        // one place the recorded layout and the served one may differ. A level recorded row-major
        // whose memberships turn out to overlap has no label column to compose, and the fallback is
        // the artifact-major route, which is correct and merely slower than the record asked for.
        // **A spatial level's membership is not a column and not a bitmap** — it is the declared
        // boxes, covered at the declared depth, resolved against this generation's segments. Built
        // here rather than claimed from the prefix because there is nothing durable to claim: the
        // ranges are a function of the geometry, so a fold-written copy would be stale at the first
        // flush and the derivation is what makes the membership never stale.
        if let Some(PredicateSource::Spatial(spatial)) = predicate {
            let ordinals = store.level(layer, level).map(|(o, _)| o).max();
            let count = ordinals.map_or(0, |max| max + 1);
            let ranges = crate::ranges::RangeSets::build(
                (0..count).map(|ordinal| store.shape_of(layer, level, ordinal)),
                spatial.depth,
                &spatial.extent,
                spatial.segments,
            );
            tracing::info!(
                layer = %layer,
                level,
                view = %view,
                ordinals = ranges.len(),
                ranges_per_artifact = ranges.ranges_per_artifact(),
                depth = spatial.depth,
                layout = ?ServingLayout::SpatialRanges,
                "a spatial level's ranges are derived from its declared shapes"
            );
            let rows = Arc::new(built.with_ranges(Some(Arc::new(ranges))));
            self.builds
                .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
            self.cached
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .insert(map_key, (key, Arc::clone(&rows)));
            return rows;
        }
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
            _ => self.column_for(
                prefix,
                view,
                layer,
                level,
                key.level_version,
                layout,
                &built,
            ),
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
            layout = ?rows.layout(),
            blocks_per_artifact = rows.membership().blocks_per_artifact(),
            "a level's row form and tile index are built"
        );
        self.builds
            .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        self.cached
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .insert(map_key, (key, Arc::clone(&rows)));
        rows
    }

    /// Take the fold-written index for this `(view, layer, level)` if one was adopted and its
    /// coordinate is still the one being built at.
    ///
    /// **Removed rather than borrowed.** An index belongs to one view's row form; once that form
    /// has it there is no second reader, and leaving the entry behind would hold a second copy of
    /// the level's extents for the process's life. A caller that finds nothing derives, which is
    /// the same answer at the cost the fold was trying to save.
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
        if key.prefix != prefix || key.level_version != level_version {
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
        let composed =
            RowColumn::compose(rows.membership(), rows.index().row_count(), layout).map(Arc::new);
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
    /// for the process's life.
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
        if key.prefix != prefix || key.level_version != level_version {
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
    /// ⊘ **`None` on a row-major level is answered from the row form**, which is correct here only
    /// because this stage still builds it (`crate::row_column`'s module doc). When it stops being
    /// built, this stops being optional — and the assertion that the two agree is what a change
    /// making it mandatory would be checked against.
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
        // **A spatial level has no membership to intersect and no histogram to read**: its members
        // are contiguous row ranges, so the count is a sum of masked range cardinalities — the same
        // quantity, from the structure that holds it.
        if let Some(ranges) = self.rows.ranges() {
            return ranges.masked_count(ordinal, self.mask);
        }
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
        if let Some(ranges) = self.rows.ranges() {
            return ranges.declared_size(ordinal);
        }
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
                parents: vec![None; sets.len()],
                declared: vec![Vec::new(); sets.len()],
            },
            MembershipRows {
                rows: sets.iter().map(|s| Some(Bitmap::of(s))).collect(),
                generating: vec![Vec::new(); sets.len()],
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
            ranges: None,
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
                parents: vec![None],
                declared: vec![contents.iter().map(|(_, declared)| *declared).collect()],
            },
            MembershipRows {
                rows: vec![Some(Bitmap::of(members))],
                generating: vec![contents.iter().map(|(set, _)| Bitmap::of(set)).collect()],
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
                parents: vec![None],
                declared: vec![Vec::new()],
            },
            MembershipRows {
                rows: vec![Some(Bitmap::of(members))],
                generating: vec![Vec::new()],
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
}
