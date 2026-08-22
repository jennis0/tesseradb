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
use tessera_types::layer::{ExistenceCriterion, LayerDeclaration};
use tessera_types::{EntityId, TermId};

use tessera_store::permutation::RowSpace;

use crate::compose::MaskedSet;
use crate::containment::{ContainmentAnswers, ContainmentPartition, PartitionSource};

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
/// **One snapshot.** Both halves are built from a single borrow of the [`ArtifactStore`] at a
/// single level version, so a write landing between two reads cannot leave the records describing
/// one population and the projection another — the failure mode `2026-08-21-artifact-layout-selection.md`
/// §9's first constraint names, where a membership that grew between the two reads leaves a
/// stale-narrow derived structure beside it.
#[derive(Debug, Clone, Default)]
pub struct ArtifactRows {
    records: ArtifactRecords,
    membership: MembershipRows,
    /// The containment partition, where this level has one.
    ///
    /// `None` under any plugin but the builtin — see [`crate::containment`], whose gate is settled
    /// fail-closed — and containment then stays on the masked-count route, which asks `M_auth`
    /// itself and so cannot depend on the shape of the rule that produced it.
    partition: Option<ContainmentPartition>,
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
}

impl ArtifactRows {
    /// Build both halves from one walk of one level, at one level version.
    pub fn build<'a>(
        artifacts: impl Iterator<Item = (u32, &'a ArtifactRecord)>,
        space: &RowSpace,
    ) -> Self {
        let mut records = ArtifactRecords::default();
        let mut membership = MembershipRows::default();
        for (ordinal, record) in artifacts {
            let idx = ordinal as usize;
            records.put(idx, record);
            membership.put(idx, record, space);
        }
        ArtifactRows {
            records,
            membership,
            partition: None,
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
#[derive(Debug, Clone, PartialEq, Eq)]
struct ProjectionKey {
    prefix: String,
    view: String,
    level_version: u64,
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

/// `(prefix, layer, level)` — what one cached partition is *for*. No view, because the expression
/// is over terms and no row space is involved in it.
type PartitionAddress = (String, String, u32);

/// What a cached [`ContainmentPartition`] was composed from. The view is deliberately absent — see
/// [`ArtifactProjections::partitions_held`] — so the terms are the prefix, which fixes the
/// postings, and the level's version, which fixes the records.
#[derive(Debug, Clone, PartialEq, Eq)]
struct PartitionKey {
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
            .retain(|(_, held, _), _| held != layer);
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
    // Eight, and every one is a thing a level's derived form is *of*: where it came from (prefix,
    // view, layer, level), what it is built from (the store, the row space), and what the
    // containment partition needs beside them. Bundling them would name the same eight things one
    // call earlier — the argument `serve_artifacts` already makes for its nine.
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
    ) -> Arc<ArtifactRows> {
        let key = ProjectionKey {
            prefix: prefix.to_string(),
            view: view.to_string(),
            level_version: store.level_version(layer, level),
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
        let rows = Arc::new(
            ArtifactRows::build(store.level(layer, level), space).with_partition(partition),
        );
        self.builds
            .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        self.cached
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .insert(map_key, (key, Arc::clone(&rows)));
        rows
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
        let map_key = (prefix.to_string(), layer.to_string(), level);
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
        let masked_count = self.rows.masked_count(ordinal, self.mask);
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

    /// The artifact's full membership size, in **row** terms.
    ///
    /// The proportional criterion's denominator, and the one place it is read. Taken from the row
    /// form rather than the entity form so that numerator and denominator come from the same
    /// projection: a member whose row still sits in an unfolded flush extent contributes to
    /// neither, which understates the ratio — fail-closed, and the same posture the write cycle
    /// takes for an unrebuilt member.
    fn declared_size(&self, ordinal: u32) -> u64 {
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
        }
    }

    /// Row-space memberships without a `RowSpace` to project through — these tests are about the
    /// predicate, and building a permutation would test the projection instead.
    fn rows_of(sets: &[&[u32]]) -> ArtifactRows {
        ArtifactRows {
            records: ArtifactRecords {
                attachments: vec![None; sets.len()],
                parents: vec![None; sets.len()],
                declared: vec![Vec::new(); sets.len()],
            },
            partition: None,
            membership: MembershipRows {
                rows: sets.iter().map(|s| Some(Bitmap::of(s))).collect(),
                generating: vec![Vec::new(); sets.len()],
            },
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
        ArtifactRows {
            records: ArtifactRecords {
                attachments: vec![None],
                parents: vec![None],
                declared: vec![contents.iter().map(|(_, declared)| *declared).collect()],
            },
            partition: None,
            membership: MembershipRows {
                rows: vec![Some(Bitmap::of(members))],
                generating: vec![contents.iter().map(|(set, _)| Bitmap::of(set)).collect()],
            },
        }
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
        ArtifactRows {
            records: ArtifactRecords {
                attachments: vec![Some(Attachment {
                    layer: CLUSTERS.to_string(),
                    level: 0,
                    ordinal: 3,
                    entity: CLUSTER_ENTITY,
                })],
                parents: vec![None],
                declared: vec![Vec::new()],
            },
            partition: None,
            membership: MembershipRows {
                rows: vec![Some(Bitmap::of(members))],
                generating: vec![Vec::new()],
            },
        }
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
