//! The one predicate: whether an artifact is served to this principal, and the number beside it.

use std::sync::Arc;

use croaring::Bitmap;
use rustc_hash::FxHashSet;

use mosaica_lifecycle::membership::Attachment;
use mosaica_lifecycle::Overlay;
use mosaica_access::Label;
use mosaica_types::layer::{ExistenceCriterion, LayerDeclaration};
use mosaica_types::EntityId;


use crate::compose::MaskedSet;
use crate::containment::ContainmentAnswers;
use crate::figures::Figures;

use super::rows::satisfied_rank_over;
use super::*;

/// Why an artifact is not served. Every variant gives a caller the same outcome, absence: a route
/// that reported which one applied would disclose exactly the facts this predicate withholds.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Withheld {
    /// No artifact at that ordinal in this view: a hole, past the level's end, or another view's.
    NoArtifact,
    /// The artifact's own entity is deleted or suppressed.
    Verdict,
    /// The viewer may not know the layer exists.
    LayerGate,
    /// The artifact's own access label, or the layer's default for one with none, does not admit
    /// this viewer.
    OwnLabel,
    /// The masked count does not clear the declared criterion.
    Criterion,
    /// What this artifact attaches to is suppressed, deleted, or in a layer this viewer does not
    /// reach: a label does not outlive the thing it labels.
    Attachment,
    /// The artifact carries supplied content and this viewer contains no content's generating set.
    Containment,
}

/// The outcome of the one predicate.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ArtifactVerdict {
    /// Served, with this masked count beside it and, where the layer declares content, the
    /// position in the artifact's content row of the one content this viewer gets, entire: the
    /// content the verdict's own version of the record ranked, whatever has been withdrawn since.
    Serve {
        masked_count: u64,
        position: Option<u32>,
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

/// The access-label test for one layer's artifacts and one viewer: an artifact with labels is
/// admitted when the terms the viewer's credential holds satisfy any of them, and one with none
/// by `unlabelled`, which the layer's `artifact_visibility.default` decided. It reads the
/// credential and nothing a request can narrow, so a filter never moves it.
#[derive(Clone, Copy)]
pub struct LabelGate<'a> {
    /// The terms the viewer holds, or `None` for a viewer holding every term.
    held: Option<&'a FxHashSet<Vec<u8>>>,
    unlabelled: bool,
}

impl<'a> LabelGate<'a> {
    pub fn new(held: &'a FxHashSet<Vec<u8>>, unlabelled: bool) -> Self {
        LabelGate {
            held: Some(held),
            unlabelled,
        }
    }

    /// The gate of a viewer holding every term, which satisfies every label that parses.
    pub fn every_term(unlabelled: bool) -> Self {
        LabelGate {
            held: None,
            unlabelled,
        }
    }

    /// Whether an artifact carrying `access` is admitted. A label that did not parse admits
    /// nobody.
    pub fn admits(&self, access: &[Option<Label>]) -> bool {
        if access.is_empty() {
            return self.unlabelled;
        }
        let held = |term: &str| self.held.is_none_or(|held| held.contains(term.as_bytes()));
        access
            .iter()
            .flatten()
            .any(|label| label.satisfied_by(&held))
    }
}

/// Everything the predicate needs about one viewer and one layer, gathered once: what is global
/// to the layer, and one [`ArtifactSide`] per point shard of the view.
///
/// Every number the verdict reads is summed over the sides before anything is decided: the
/// masked count, the proportional criterion's denominator, and per rank the generating set's
/// size and the viewer's holding of it. No criterion or containment test is applied within one
/// shard.
pub struct ArtifactView<'a, M: MaskedSet> {
    pub declaration: &'a LayerDeclaration,
    pub overlay: &'a Overlay,
    /// The access-label test for this layer's artifacts and this viewer.
    pub labels: LabelGate<'a>,
    /// Whether the viewer reaches the layer at all, resolved once per session. The overlay half of
    /// the verdict is still asked live, never cached alongside this.
    pub layer_reachable: bool,
    /// Is the artifact this one attaches to served to this viewer? A hook, not a resolved answer,
    /// because the target lives in another layer and answering means that layer's own
    /// reachability, suppression, row form and declaration.
    ///
    /// Every failure to answer is `false`: an unreached target layer, a layer dropped since
    /// resolution, an absent level and a target simply not shown are one answer, because which
    /// applies is exactly the fact being withheld. Nothing about the target is cached across the
    /// call, so a suppression accepted since is still honoured here.
    pub dependency_served: &'a dyn Fn(&Attachment) -> bool,
    /// One per point shard holding the view, seen or not.
    pub sides: Vec<ArtifactSide<'a, M>>,
}

/// One point shard's part of [`ArtifactView`]: its form of the level, and the viewer's mask, the
/// deny set and the containment answers in that shard.
pub struct ArtifactSide<'a, M: MaskedSet> {
    /// This shard's row form of the layer's membership in the view.
    pub rows: &'a ArtifactRows,
    /// The viewer's composed mask in this shard — see [`MaskedSet`] for why the type forbids
    /// anything else — or `None` where the request left the shard out.
    pub mask: Option<&'a M>,
    /// `deleted ∪ suppressed`, in this shard's row space for the view. Read only by the
    /// containment partition's fast arm, to correct an answer the partition derives from term
    /// signatures alone, which a deletion or suppression does not touch; re-derived at every
    /// acknowledgement rather than subtracted from, so `delete → suppress → unsuppress` leaves the
    /// entity deleted.
    pub denied: &'a Bitmap,
    /// This principal's answers over this shard's containment partition, where the level has one.
    /// A fast arm, never a second rule: `None` puts every artifact on the masked-count route;
    /// `Some` answers from terms and asks the mask only for the deny correction. The two must agree
    /// rank for rank.
    pub containment: Option<ContainmentAnswers<'a>>,
    /// This session's masked counts over this shard's form of the level, where it is served
    /// row-major, held per `(session, layer)` and byte-budgeted. `None` on an artifact-major level,
    /// which answers `|membership ∩ M_auth|` one artifact at a time and has no per-session
    /// structure to buy.
    pub counts: Option<Arc<Figures>>,
}

impl<'a, M: MaskedSet> ArtifactView<'a, M> {
    /// The view over `layer` for `served`'s viewer, who reaches it: the layer's declaration and
    /// its space's overlay, and `answer` for each of the view's sides, given its place, so no
    /// shard can be left out of a sum.
    pub(crate) fn of(
        served: &crate::viewport::ServedView<'a>,
        layer: &'a mosaica_types::layer::RegisteredLayer,
        labels: LabelGate<'a>,
        dependency_served: &'a dyn Fn(&Attachment) -> bool,
        mut answer: impl FnMut(usize, &crate::viewport::ServedSide<'a>) -> ArtifactSide<'a, M>,
    ) -> Self {
        ArtifactView {
            declaration: &layer.declaration,
            overlay: served.generation.space_overlay(layer.space),
            labels,
            layer_reachable: true,
            dependency_served,
            sides: served
                .sides
                .iter()
                .enumerate()
                .map(|(place, side)| answer(place, side))
                .collect(),
        }
    }
}

impl<M: MaskedSet> ArtifactView<'_, M> {
    /// The one predicate.
    pub fn verdict(&self, artifact_entity: EntityId, ordinal: u32) -> ArtifactVerdict {
        if let Some(withheld) = self.withheld_before_count(artifact_entity, ordinal) {
            return ArtifactVerdict::Absent(withheld);
        }

        // Against the live masked count, the same number returned to the caller.
        let masked_count = self.masked_count(ordinal);
        if !self.clears(ordinal, masked_count) {
            return ArtifactVerdict::Absent(Withheld::Criterion);
        }

        match self.position(ordinal) {
            Ok(position) => ArtifactVerdict::Serve {
                masked_count,
                position,
            },
            Err(withheld) => ArtifactVerdict::Absent(withheld),
        }
    }

    /// [`Self::verdict`] for an artifact the caller has seen hold a member this viewer may see,
    /// answering with the position of the content served. A count criterion of at most one is then
    /// met without counting; any other criterion is counted as the verdict counts it.
    pub fn serves_visible_member(
        &self,
        artifact_entity: EntityId,
        ordinal: u32,
    ) -> Result<Option<u32>, Withheld> {
        if let Some(withheld) = self.withheld_before_count(artifact_entity, ordinal) {
            return Err(withheld);
        }
        let counted = match self.declaration.require_member_visibility {
            None | Some(ExistenceCriterion::Count(0 | 1)) => true,
            Some(_) => self.clears(ordinal, self.masked_count(ordinal)),
        };
        if !counted {
            return Err(Withheld::Criterion);
        }
        self.position(ordinal)
    }

    /// The level's records, the same in every side.
    fn records(&self) -> Option<&ArtifactRecords> {
        self.sides.first().map(|side| side.rows.records())
    }

    /// Whether every side's form is of one version of the level: one set of records is what every
    /// shard's form at a version shares. Sides of two versions could sum a set's slices from two
    /// versions of it to its size.
    fn one_version(&self) -> bool {
        self.sides
            .windows(2)
            .all(|pair| Arc::ptr_eq(&pair[0].rows.records, &pair[1].rows.records))
    }

    /// Every test the verdict makes before it reads the masked count, in its order.
    fn withheld_before_count(&self, artifact_entity: EntityId, ordinal: u32) -> Option<Withheld> {
        // Holes, ordinals past the level's end and another view's group-scoped artifact are absent.
        let Some(first) = self.sides.first().filter(|side| side.rows.holds(ordinal)) else {
            return Some(Withheld::NoArtifact);
        };
        let records = first.rows.records();

        // The overlay, first and unconditional, asked live so a suppression takes effect at once.
        if self.overlay.is_deleted(artifact_entity) || self.overlay.is_suppressed(artifact_entity) {
            return Some(Withheld::Verdict);
        }

        // The layer's gate.
        if !self.layer_reachable {
            return Some(Withheld::LayerGate);
        }

        // The artifact's own label, before anything reads its membership or its target.
        if !self.labels.admits(records.access(ordinal)) {
            return Some(Withheld::OwnLabel);
        }

        // Before the criterion, so a withheld dependency makes this absent without its own
        // membership being touched, on every route: search, a held identifier and a filter reach
        // an attached artifact directly rather than by traversing the edge.
        if let Some(attachment) = records.attachment(ordinal) {
            if !(self.dependency_served)(attachment) {
                return Some(Withheld::Attachment);
            }
        }
        None
    }

    /// Whether `masked_count` clears the layer's existence criterion.
    fn clears(&self, ordinal: u32, masked_count: u64) -> bool {
        match self.declaration.require_member_visibility {
            None => true,
            Some(ExistenceCriterion::Count(n)) => masked_count >= n,
            // A zero denominator cannot clear a positive fraction.
            Some(ExistenceCriterion::Fraction(p)) => {
                let declared = self.declared_size(ordinal);
                declared > 0 && (masked_count as f64) >= p * (declared as f64)
            }
        }
    }

    /// The verdict's last test: the first content whose generating set this viewer holds
    /// entirely, by its position in the content row, or nothing where the layer declares none.
    fn position(&self, ordinal: u32) -> Result<Option<u32>, Withheld> {
        match self.containment(ordinal) {
            Containment::NothingToContain => Ok(None),
            Containment::Satisfied(rank) => match self.records() {
                Some(records) => Ok(Some(records.position(ordinal, rank))),
                None => Err(Withheld::Containment),
            },
            Containment::Unsatisfied => Err(Withheld::Containment),
        }
    }

    /// Whether the artifact's own label admits this viewer. Reads no membership, so a caller may
    /// ask it before any masked probe.
    pub fn admits_label(&self, ordinal: u32) -> bool {
        self.records()
            .is_some_and(|records| self.labels.admits(records.access(ordinal)))
    }

    /// Containment over every side, each by its partition where that answers and by its mask where
    /// it does not. A side whose partition cannot answer a rank is asked again from the mask,
    /// every rank of the ordinal, as though it had no partition. Sides of more than one version of
    /// the level contain nothing.
    fn containment(&self, ordinal: u32) -> Containment {
        let declares_content = !self.declaration.content.supplied.is_empty();
        if !self.one_version() {
            return Containment::Unsatisfied;
        }
        let declared = self.records().map_or(&[][..], |r| r.declared_sizes(ordinal));
        // Empty where no side has a partition: only a side with one reads it.
        let mut via: Vec<bool> = match self.sides.iter().any(|side| side.containment.is_some()) {
            true => self
                .sides
                .iter()
                .map(|side| {
                    side.containment
                        .as_ref()
                        .is_some_and(|answers| side.rows.partition_answers(ordinal, answers))
                })
                .collect(),
            false => Vec::new(),
        };
        loop {
            let mut declined = None;
            let answer = satisfied_rank_over(
                declared,
                declares_content,
                self.sides.len(),
                |place, rank| self.sides[place].rows.generating_size(ordinal, rank),
                |place, rank, size| {
                    let side = &self.sides[place];
                    let Some(mask) = side.mask else {
                        return Some(size == 0);
                    };
                    match side.containment.as_ref().filter(|_| via[place]) {
                        Some(answers) => {
                            let held =
                                side.rows.generating_held_via(ordinal, rank, answers, side.denied);
                            if held.is_none() {
                                declined = Some(place);
                            }
                            held
                        }
                        None => Some(side.rows.generating_held(ordinal, rank, mask) == size),
                    }
                },
            );
            match (answer, declined) {
                (Some(containment), _) => return containment,
                (None, Some(place)) => via[place] = false,
                (None, None) => return Containment::Unsatisfied,
            }
        }
    }

    /// The masked count over every side. An artifact-major level intersects each side's own
    /// membership with that side's mask; a row-major level has no per-artifact membership, and
    /// takes each side's count from the level's masked counts there.
    fn masked_count(&self, ordinal: u32) -> u64 {
        self.sides
            .iter()
            .map(|side| {
                let Some(mask) = side.mask else {
                    return 0;
                };
                if side.rows.layout().is_row_major() {
                    if let Some(counts) = &side.counts {
                        return counts.get(ordinal);
                    }
                }
                side.rows.masked_count(ordinal, mask)
            })
            .sum()
    }

    /// The proportional criterion's denominator over every side, in row terms so numerator and
    /// denominator come from the same projections: a member still in an unfolded flush extent
    /// contributes to neither, understating rather than overstating the ratio.
    fn declared_size(&self, ordinal: u32) -> u64 {
        self.sides
            .iter()
            .map(|side| match side.rows.column() {
                Some(column) => column.declared_size(ordinal),
                None => side.rows.get(ordinal).map(Bitmap::cardinality).unwrap_or(0),
            })
            .sum()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::artifacts::test_support::*;
    use mosaica_lifecycle::wal::ChangeOp;

    /// A dependency not served to this viewer, for whatever reason.
    fn dependency_absent(_attachment: &Attachment) -> bool {
        false
    }

    /// Two principals, one artifact, two different counts, and neither is the artifact's size. A
    /// count equal to the membership would mean the mask was never applied.
    #[test]
    fn the_count_is_the_viewers_own_and_never_the_artifacts_size() {
        let d = declaration(false, None);
        let members: &[&[u32]] = &[&[1, 2, 3, 4, 5, 6, 7, 8, 9, 10]];

        let broad = Fixture::new(members, &[1, 2, 3, 4, 5, 6, 7, 8]);
        let narrow = Fixture::new(members, &[9, 10, 11, 12]);

        let broad_count = broad
            .view(&d, true)
            .verdict(EntityId::new(999), 0)
            .masked_count()
            .unwrap();
        let narrow_count = narrow
            .view(&d, true)
            .verdict(EntityId::new(999), 0)
            .masked_count()
            .unwrap();

        assert_eq!(broad_count, 8);
        assert_eq!(narrow_count, 2);
        assert_ne!(broad_count, 10, "the declared size must never be served");
        assert_ne!(narrow_count, 10);
    }

    /// The criterion decides existence and never modifies a number: an artifact that clears it is
    /// served with the same count that was tested.
    #[test]
    fn the_criterion_decides_existence_and_leaves_the_count_alone() {
        let d = declaration(false, Some(ExistenceCriterion::Count(5)));
        let fx = Fixture::new(&[&[1, 2, 3, 4, 5, 6, 7, 8, 9, 10]], &[1, 2, 3, 4, 5, 6]);
        assert_eq!(
            fx.view(&d, true).verdict(EntityId::new(999), 0),
            ArtifactVerdict::Serve {
                masked_count: 6,
                position: None
            }
        );

        // One fewer visible member: absent, not served with a rounded count.
        let below = Fixture::new(&[&[1, 2, 3, 4, 5, 6, 7, 8, 9, 10]], &[1, 2, 3, 4]);
        assert_eq!(
            below.view(&d, true).verdict(EntityId::new(999), 0),
            ArtifactVerdict::Absent(Withheld::Criterion)
        );
    }

    /// The proportional form divides by the declared size, so the same masked count can pass on a
    /// small artifact and fail on a large one.
    #[test]
    fn the_proportional_criterion_scales_where_the_absolute_one_does_not() {
        let d = declaration(false, Some(ExistenceCriterion::Fraction(0.5)));

        let small: Vec<u32> = (0..10).collect();
        let large: Vec<u32> = (0..1000).collect();
        let mask: Vec<u32> = (0..6).collect();

        let fx = Fixture::new(&[&small, &large], &mask);
        let view = fx.view(&d, true);
        assert!(view.verdict(EntityId::new(999), 0).is_served());
        assert_eq!(
            view.verdict(EntityId::new(998), 1),
            ArtifactVerdict::Absent(Withheld::Criterion)
        );
    }

    /// The overlay is first and unconditional: a suppressed artifact is absent regardless.
    #[test]
    fn a_suppression_beats_every_other_conjunct() {
        let d = declaration(false, None);
        let mut fx = Fixture::new(&[&[1, 2, 3]], &[1, 2, 3]);
        let entity = EntityId::new(999);
        assert!(fx.view(&d, true).verdict(entity, 0).is_served());

        fx.overlay.apply(entity, ChangeOp::Suppress);
        assert_eq!(
            fx.view(&d, true).verdict(entity, 0),
            ArtifactVerdict::Absent(Withheld::Verdict)
        );

        let mut fx = Fixture::new(&[&[1, 2, 3]], &[1, 2, 3]);
        fx.overlay.apply(entity, ChangeOp::Delete);
        assert_eq!(
            fx.view(&d, true).verdict(entity, 0),
            ArtifactVerdict::Absent(Withheld::Verdict)
        );
    }

    /// The own label and the criterion are independent declarations composed by conjunction:
    /// satisfying one does not switch the other off.
    #[test]
    fn the_own_label_does_not_disable_the_criterion() {
        let d = declaration(true, Some(ExistenceCriterion::Count(5)));
        let mut fx = Fixture::new(&[&[1, 2, 3, 4, 5, 6, 7, 8, 9, 10]], &[1, 2, 3, 4]);
        fx.held.insert(b"t7".to_vec());
        fx.label(0, &[b"t7"]);
        assert_eq!(
            fx.view(&d, true).verdict(EntityId::new(999), 0),
            ArtifactVerdict::Absent(Withheld::Criterion)
        );
    }

    /// An artifact's own label admits a viewer holding any of its descriptors and no other, and
    /// the label is tested before the criterion reads the membership.
    #[test]
    fn an_artifact_is_served_only_to_a_viewer_its_own_label_admits() {
        let d = declaration(true, Some(ExistenceCriterion::Count(50)));
        let mut fx = Fixture::new(&[&[1, 2, 3], &[1, 2, 3]], &[1, 2, 3]);
        fx.label(0, &[b"a", b"b"]);
        fx.label(1, &[b"c"]);
        fx.held.insert(b"b".to_vec());
        assert_eq!(
            fx.view(&d, true).verdict(EntityId::new(999), 1),
            ArtifactVerdict::Absent(Withheld::OwnLabel),
            "a label the viewer does not hold withholds before the criterion is asked"
        );
        let d = declaration(true, None);
        assert!(fx.view(&d, true).verdict(EntityId::new(998), 0).is_served());
        assert_eq!(
            fx.view(&d, true).verdict(EntityId::new(999), 1),
            ArtifactVerdict::Absent(Withheld::OwnLabel)
        );
    }

    /// An artifact with no label is answered by the layer's default, which the caller settles
    /// into `unlabelled`.
    #[test]
    fn an_unlabelled_artifact_takes_the_layers_default() {
        let d = declaration(true, None);
        let mut fx = Fixture::new(&[&[1, 2, 3]], &[1, 2, 3]);
        assert!(fx.view(&d, true).verdict(EntityId::new(999), 0).is_served());
        fx.unlabelled = false;
        assert_eq!(
            fx.view(&d, true).verdict(EntityId::new(999), 0),
            ArtifactVerdict::Absent(Withheld::OwnLabel)
        );
    }

    /// A label-withheld artifact never asks after the artifact it attaches to.
    #[test]
    fn the_own_label_precedes_the_attachment() {
        let d = declaration(true, None);
        let mut rows = attached_rows(&[1, 2, 3]);
        let records = Arc::make_mut(&mut rows.records);
        records.access = vec![Some(super::super::rows::parse_access(&[b"x".to_vec()]))];
        let mask = Bitmap::of(&[1, 2, 3]);
        let never = |_: &Attachment| -> bool { panic!("a withheld label asked its target") };
        let held = FxHashSet::default();
        assert_eq!(
            ArtifactView {
                declaration: &d,
                overlay: &Overlay::new(),
                labels: LabelGate::new(&held, true),
                layer_reachable: true,
                dependency_served: &never,
                sides: vec![side(&rows, Some(&mask))],
            }
            .verdict(LABEL_ENTITY, 0),
            ArtifactVerdict::Absent(Withheld::OwnLabel)
        );
    }

    /// An unreachable layer withholds every artifact in it, before any membership is touched.
    #[test]
    fn an_unreachable_layer_withholds_its_artifacts() {
        let d = declaration(false, None);
        let fx = Fixture::new(&[&[1, 2, 3]], &[1, 2, 3]);
        assert_eq!(
            fx.view(&d, false).verdict(EntityId::new(999), 0),
            ArtifactVerdict::Absent(Withheld::LayerGate)
        );
    }

    // ---- containment ------------------------------------------------------------------------

    /// Containment is all or nothing, not a coverage fraction.
    #[test]
    fn a_viewer_missing_one_member_of_the_generating_set_is_served_nothing() {
        let d = declaration(false, None);
        let generating: &[u32] = &[10, 11, 12, 13];
        let rows = rows_with_contents(&[1, 2, 3, 10, 11, 12, 13], &[(generating, 4)]);

        let all = Bitmap::of(&[1, 2, 3, 10, 11, 12, 13]);
        let view = ArtifactView {
            declaration: &d,
            overlay: &Overlay::new(),
            labels: open_labels(),
            layer_reachable: true,
            dependency_served: &dependency_served,
            sides: vec![side(&rows, Some(&all))],
        };
        assert_eq!(
            view.verdict(EntityId::new(999), 0),
            ArtifactVerdict::Serve {
                masked_count: 7,
                position: Some(0)
            }
        );

        // Holds three of four, and a larger visible set overall: what decides is which documents.
        let nearly = Bitmap::of(&[1, 2, 3, 4, 5, 6, 7, 8, 9, 10, 11, 12]);
        let view = ArtifactView {
            declaration: &d,
            overlay: &Overlay::new(),
            labels: open_labels(),
            layer_reachable: true,
            dependency_served: &dependency_served,
            sides: vec![side(&rows, Some(&nearly))],
        };
        assert_eq!(
            view.verdict(EntityId::new(999), 0),
            ArtifactVerdict::Absent(Withheld::Containment)
        );
    }

    /// The ranking is the caller's: the first content the viewer contains is the one they get.
    #[test]
    fn the_first_content_the_viewer_contains_is_the_one_they_get() {
        let d = declaration(false, None);
        let rows = rows_with_contents(
            &[1, 2, 3, 4, 5, 6],
            &[(&[1, 2, 3, 4, 5, 6], 6), (&[1, 2], 2)],
        );
        let verdict = |mask: &Bitmap| {
            ArtifactView {
                declaration: &d,
                overlay: &Overlay::new(),
                labels: open_labels(),
                layer_reachable: true,
                dependency_served: &dependency_served,
                sides: vec![side(&rows, Some(mask))],
            }
            .verdict(EntityId::new(999), 0)
        };

        for mask in [Bitmap::of(&[1, 2, 3, 4]), Bitmap::of(&[1, 2, 5, 6])] {
            match verdict(&mask) {
                ArtifactVerdict::Serve { position, .. } => assert_eq!(position, Some(1)),
                other => panic!("expected the narrow content, got {other:?}"),
            }
        }
        match verdict(&Bitmap::of(&[1, 2, 3, 4, 5, 6])) {
            ArtifactVerdict::Serve { position, .. } => assert_eq!(position, Some(0)),
            other => panic!("expected the ranked-first content, got {other:?}"),
        }
        assert_eq!(
            verdict(&Bitmap::of(&[3, 4])),
            ArtifactVerdict::Absent(Withheld::Containment)
        );
    }

    /// A generating set that lost members in projection fails for everybody.
    #[test]
    fn a_generating_set_that_did_not_survive_projection_contains_nobody() {
        let d = declaration(false, None);
        let rows = rows_with_contents(&[1, 2, 3], &[(&[1, 2], 4)]);
        let everything = Bitmap::from_range(0..1000);
        let view = ArtifactView {
            declaration: &d,
            overlay: &Overlay::new(),
            labels: open_labels(),
            layer_reachable: true,
            dependency_served: &dependency_served,
            sides: vec![side(&rows, Some(&everything))],
        };
        assert_eq!(
            view.verdict(EntityId::new(999), 0),
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
                    space: mosaica_types::LayerSpace::ZERO,
                    level: 0,
                    ordinal: 3,
                    entity: CLUSTER_ENTITY,
                })],
                parents: vec![Vec::new()],
                declared: vec![Vec::new()],
                withdrawn: Vec::new(),
                access: Vec::new(),
                attached_by: Default::default(),
            },
            MembershipRows {
                rows: vec![Some(Arc::new(Bitmap::of(members)))],
                generating: vec![Vec::new()],
                rows_held: true,
            },
        )
    }

    /// A label whose cluster is not served to this viewer is absent, with every other conjunct
    /// passing. Whether the cluster was suppressed, deleted, folded away, gated or below its own
    /// bar arrives here as the same `false`.
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
                labels: open_labels(),
                layer_reachable: true,
                dependency_served: prerequisite,
                sides: vec![side(&rows, Some(&mask))],
            }
            .verdict(LABEL_ENTITY, 0)
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

    /// A suppression of the label's own entity beats everything, prerequisite included.
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
                labels: open_labels(),
                layer_reachable: true,
                dependency_served: &dependency_served,
                sides: vec![side(&rows, Some(&mask))],
            }
            .verdict(LABEL_ENTITY, 0),
            ArtifactVerdict::Absent(Withheld::Verdict)
        );
    }

    /// The prerequisite is asked before the artifact's own membership is looked at.
    #[test]
    fn the_prerequisite_precedes_the_labels_own_criterion() {
        let d = declaration(false, Some(ExistenceCriterion::Count(50)));
        let rows = attached_rows(&[1, 2, 3]);
        let mask = Bitmap::of(&[1, 2, 3]);
        assert_eq!(
            ArtifactView {
                declaration: &d,
                overlay: &Overlay::new(),
                labels: open_labels(),
                layer_reachable: true,
                dependency_served: &dependency_absent,
                sides: vec![side(&rows, Some(&mask))],
            }
            .verdict(LABEL_ENTITY, 0),
            ArtifactVerdict::Absent(Withheld::Attachment)
        );
    }

    /// An artifact that depends on nothing asks nothing.
    #[test]
    fn an_unattached_artifact_never_consults_the_prerequisite() {
        let d = declaration(false, None);
        let rows = rows_of(&[&[1, 2, 3]]);
        let mask = Bitmap::of(&[1, 2, 3]);
        let never =
            |_: &Attachment| -> bool { panic!("an unattached artifact asked its dependency") };
        assert!(ArtifactView {
            declaration: &d,
            overlay: &Overlay::new(),
            labels: open_labels(),
            layer_reachable: true,
            dependency_served: &never,
            sides: vec![side(&rows, Some(&mask))],
        }
        .verdict(EntityId::new(999), 0)
        .is_served());
    }

    /// A layer declaring no supplied content has nothing to contain.
    #[test]
    fn an_artifact_with_no_contents_has_nothing_to_contain() {
        let d = declaration(false, None);
        let fx = Fixture::new(&[&[1, 2, 3]], &[1, 2, 3]);
        assert_eq!(
            fx.view(&d, true).verdict(EntityId::new(999), 0),
            ArtifactVerdict::Serve {
                masked_count: 3,
                position: None
            }
        );
    }

    // ---- several shards ---------------------------------------------------------------------

    /// One artifact's forms in two shards: its generating set of four members is split two and
    /// two, and each form holds its own slice of the membership.
    fn split() -> (ArtifactRows, ArtifactRows) {
        one_level(
            rows_with_contents(&[1, 2, 3], &[(&[1, 2], 4)]),
            rows_with_contents(&[7, 8], &[(&[7, 8], 4)]),
        )
    }

    /// Two shards' forms of one version of a level, holding one set of records as every shard's
    /// form at a version does.
    fn one_level(a: ArtifactRows, mut b: ArtifactRows) -> (ArtifactRows, ArtifactRows) {
        b.records = Arc::clone(&a.records);
        (a, b)
    }

    /// A side left out adds its slice and nothing held, so the test fails closed; an empty slice
    /// never contains the set alone.
    #[test]
    fn a_split_generating_set_is_contained_only_by_the_sums_over_every_side() {
        let d = declaration(false, None);
        let (a, b) = split();
        let (all_a, all_b) = (Bitmap::of(&[1, 2, 3]), Bitmap::of(&[7, 8]));
        assert_eq!(
            view_of(&d, vec![side(&a, Some(&all_a)), side(&b, Some(&all_b))])
                .verdict(EntityId::new(999), 0),
            ArtifactVerdict::Serve {
                masked_count: 5,
                position: Some(0)
            },
            "both slices whole: served, with the count summed over the sides"
        );

        let short = Bitmap::of(&[7]);
        let left_out = [
            vec![side(&a, Some(&all_a)), side(&b, Some(&short))],
            vec![side(&a, Some(&all_a)), side(&b, None)],
            vec![side(&a, Some(&all_a))],
        ];
        for sides in left_out {
            assert_eq!(
                view_of(&d, sides).verdict(EntityId::new(999), 0),
                ArtifactVerdict::Absent(Withheld::Containment),
                "a slice the viewer does not hold whole, or a side not asked, contains nothing"
            );
        }

        // A shard holding none of a set of two, and one holding both.
        let (empty, whole) = one_level(
            rows_with_contents(&[1], &[(&[], 2)]),
            rows_with_contents(&[7, 8], &[(&[7, 8], 2)]),
        );
        let everything = Bitmap::from_range(0..100);
        assert_eq!(
            view_of(&d, vec![side(&empty, Some(&everything))]).verdict(EntityId::new(999), 0),
            ArtifactVerdict::Absent(Withheld::Containment),
            "an empty slice is never the set"
        );
        assert!(view_of(
            &d,
            vec![side(&empty, Some(&everything)), side(&whole, Some(&everything))]
        )
        .verdict(EntityId::new(999), 0)
        .is_served());
    }

    #[test]
    fn a_split_artifact_is_served_only_on_both_shards_counts() {
        let d = declaration(false, Some(ExistenceCriterion::Count(4)));
        let (a, b) = one_level(rows_of(&[&[1, 2, 3]]), rows_of(&[&[1, 2, 5]]));
        let (seen_a, seen_b) = (Bitmap::of(&[1, 2, 9]), Bitmap::of(&[2, 5]));
        assert_eq!(
            view_of(&d, vec![side(&a, Some(&seen_a)), side(&b, Some(&seen_b))])
                .verdict(EntityId::new(999), 0),
            ArtifactVerdict::Serve {
                masked_count: 4,
                position: None
            }
        );
        for alone in [
            vec![side(&a, Some(&seen_a)), side(&b, None)],
            vec![side(&a, None), side(&b, Some(&seen_b))],
        ] {
            assert_eq!(
                view_of(&d, alone).verdict(EntityId::new(999), 0),
                ArtifactVerdict::Absent(Withheld::Criterion)
            );
        }
        let operand = [a.visible_rows(0, &seen_a), b.visible_rows(0, &seen_b)];
        assert_eq!(operand, [Bitmap::of(&[1, 2]), Bitmap::of(&[2, 5])]);
        assert_eq!(operand.iter().map(Bitmap::cardinality).sum::<u64>(), 4);
    }

    /// The partition's arm answers per side, over that side's slice, and the mask answers a side
    /// with no partition: the two sides' answers are summed as the mask's would be.
    #[test]
    fn each_side_answers_containment_by_its_own_partition_or_its_own_mask() {
        let d = declaration(false, None);
        let (a, b) = split();
        // Side a's partition, over its slice of two members, each reached by term 7.
        let a = a.with_partition(Some(crate::containment::ContainmentPartition::of_sets(&[&[
            (2, &[&[7], &[7]]),
        ]])));
        let partition = a.partition().expect("the fixture attached one");
        let held = [mosaica_types::TermId::new(7)]
            .into_iter()
            .collect::<rustc_hash::FxHashSet<_>>();
        let (all_a, all_b) = (Bitmap::of(&[1, 2, 3]), Bitmap::of(&[7, 8]));
        let short = Bitmap::of(&[8]);
        let by_terms = |b_mask: Option<&Bitmap>, denied: &Bitmap| {
            let mut a_side = side(&a, Some(&all_a));
            a_side.containment = Some(partition.answers(&held));
            a_side.denied = denied;
            view_of(&d, vec![a_side, side(&b, b_mask)]).verdict(EntityId::new(999), 0)
        };
        assert!(by_terms(Some(&all_b), no_denies()).is_served());
        assert_eq!(
            by_terms(Some(&short), no_denies()),
            ArtifactVerdict::Absent(Withheld::Containment)
        );
        assert_eq!(
            by_terms(None, no_denies()),
            ArtifactVerdict::Absent(Withheld::Containment)
        );
        let denied = Bitmap::of(&[2]);
        assert_eq!(
            by_terms(Some(&all_b), &denied),
            ArtifactVerdict::Absent(Withheld::Containment),
            "a denied member of one side's slice fails the whole set"
        );
    }

    /// Leaving a left-out side out of the denominator would let a viewer who sees nothing in one
    /// shard clear a fraction of the members there.
    #[test]
    fn the_fraction_is_against_every_sides_members() {
        let (a, b) = one_level(
            rows_of(&[&(1..=10).collect::<Vec<u32>>()]),
            rows_of(&[&(20..=29).collect::<Vec<u32>>()]),
        );
        let (all_a, none) = (Bitmap::from_range(0..100), Bitmap::new());
        for (p, served) in [(0.6, false), (0.5, true)] {
            let d = declaration(false, Some(ExistenceCriterion::Fraction(p)));
            for b_mask in [Some(&none), None] {
                let verdict = view_of(&d, vec![side(&a, Some(&all_a)), side(&b, b_mask)])
                    .verdict(EntityId::new(999), 0);
                assert_eq!(verdict.is_served(), served, "{p} of twenty members, ten visible");
                if served {
                    assert_eq!(verdict.masked_count(), Some(10));
                }
            }
        }
        let d = declaration(false, Some(ExistenceCriterion::Fraction(0.6)));
        assert!(
            view_of(&d, vec![side(&a, Some(&all_a))])
                .verdict(EntityId::new(999), 0)
                .is_served(),
            "the same ten of ten in one shard clear it"
        );
    }

    /// Sides of two versions of a level contain nothing: a set's slices from two versions of it
    /// could sum to its size without being any one version's set.
    #[test]
    fn sides_of_two_versions_of_a_level_contain_nothing() {
        let d = declaration(false, None);
        let (a, _) = split();
        let b = rows_with_contents(&[7, 8], &[(&[7, 8], 4)]);
        let (all_a, all_b) = (Bitmap::of(&[1, 2, 3]), Bitmap::of(&[7, 8]));
        assert_eq!(
            view_of(&d, vec![side(&a, Some(&all_a)), side(&b, Some(&all_b))])
                .verdict(EntityId::new(999), 0),
            ArtifactVerdict::Absent(Withheld::Containment)
        );
    }
}
