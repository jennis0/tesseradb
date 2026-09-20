//! The one predicate: whether an artifact is served to this principal, and the number beside it.

use std::sync::Arc;

use croaring::Bitmap;
use rustc_hash::FxHashSet;

use tessera_lifecycle::membership::Attachment;
use tessera_lifecycle::Overlay;
use tessera_types::layer::{ExistenceCriterion, LayerDeclaration};
use tessera_types::{EntityId, TermId};


use crate::compose::MaskedSet;
use crate::containment::ContainmentAnswers;
use crate::histogram::MaskedCounts;

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
    /// The layer says its artifacts carry their own terms, and this viewer holds none of this one's.
    OwnTerms,
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
    /// Served, with this masked count beside it and, where the layer declares content, the rank of
    /// the one content this viewer gets, entire.
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

/// Everything the predicate needs about one viewer and one layer, gathered once.
pub struct ArtifactView<'a, M: MaskedSet> {
    pub declaration: &'a LayerDeclaration,
    pub overlay: &'a Overlay,
    /// The viewer's satisfied terms, the same set item visibility uses. Satisfaction is
    /// intersection with this set, never a conservative label join, which would yield an empty
    /// required set for a disjunctive gate and admit every principal.
    pub satisfied: &'a FxHashSet<TermId>,
    /// Whether the viewer reaches the layer at all, resolved once per session. The overlay half of
    /// the verdict is still asked live, never cached alongside this.
    pub layer_reachable: bool,
    /// This view's row form of the layer's membership.
    pub rows: &'a ArtifactRows,
    /// Is the artifact this one attaches to served to this viewer? A hook, not a resolved answer,
    /// because the target lives in another layer and answering means that layer's own
    /// reachability, suppression, row form and declaration.
    ///
    /// Every failure to answer is `false`: an unreached target layer, a layer dropped since
    /// resolution, an absent level and a target simply not shown are one answer, because which
    /// applies is exactly the fact being withheld. Nothing about the target is cached across the
    /// call, so a suppression accepted since is still honoured here.
    pub dependency_served: &'a dyn Fn(&Attachment) -> bool,
    /// The viewer's composed mask — see [`MaskedSet`] for why the type forbids anything else.
    pub mask: &'a M,
    /// `deleted ∪ suppressed`, in this view's row space. Read only by the containment partition's
    /// fast arm, to correct an answer the partition derives from term signatures alone, which a
    /// deletion or suppression does not touch; re-derived at every acknowledgement rather than
    /// subtracted from, so `delete → suppress → unsuppress` leaves the entity deleted.
    pub denied: &'a Bitmap,
    /// This principal's answers over the level's containment partition, where the level has one. A
    /// fast arm, never a second rule: `None` puts every artifact on the masked-count route; `Some`
    /// answers from terms and asks the mask only for the deny correction. The two must agree rank
    /// for rank.
    pub containment: Option<ContainmentAnswers<'a>>,
    /// This session's masked counts over the level, where it is served row-major, held per
    /// `(session, layer)` and byte-budgeted. `None` on an artifact-major level, which answers
    /// `|membership ∩ M_auth|` one artifact at a time and has no per-session structure to buy.
    pub counts: Option<Arc<MaskedCounts>>,
}

impl<M: MaskedSet> ArtifactView<'_, M> {
    /// The one predicate. `own_terms` is the artifact's own access label resolved to a term, or
    /// `None` if it carries none: a layer whose `artifact_visibility` names a field withholds an
    /// artifact with no term there rather than admitting it as a grant to everyone.
    pub fn verdict(
        &self,
        artifact_entity: EntityId,
        ordinal: u32,
        own_terms: Option<TermId>,
    ) -> ArtifactVerdict {
        // Holes, ordinals past the level's end and another view's group-scoped artifact are absent.
        if !self.rows.holds(ordinal) {
            return ArtifactVerdict::Absent(Withheld::NoArtifact);
        }

        // The overlay, first and unconditional, asked live so a suppression takes effect at once.
        if self.overlay.is_deleted(artifact_entity) || self.overlay.is_suppressed(artifact_entity) {
            return ArtifactVerdict::Absent(Withheld::Verdict);
        }

        // The layer's gate.
        if !self.layer_reachable {
            return ArtifactVerdict::Absent(Withheld::LayerGate);
        }

        // Run before the artifact's own terms and criterion, so a withheld dependency makes this
        // absent without its own membership being touched — for every route, since search, a held
        // identifier and a filter reach a label directly rather than by traversing the edge.
        if let Some(attachment) = self.rows.attachment(ordinal) {
            if !(self.dependency_served)(attachment) {
                return ArtifactVerdict::Absent(Withheld::Attachment);
            }
        }

        // The artifact's own terms, if its layer says it carries them.
        if self.declaration.artifact_visibility.carries_own_labels() {
            match own_terms {
                Some(term) if self.satisfied.contains(&term) => {}
                _ => return ArtifactVerdict::Absent(Withheld::OwnTerms),
            }
        }

        // Against the live masked count, the same number returned to the caller.
        let masked_count = self.masked_count(ordinal);
        if let Some(criterion) = self.declaration.require_member_visibility {
            let clears = match criterion {
                ExistenceCriterion::Count(n) => masked_count >= n,
                // A zero denominator cannot clear a positive fraction.
                ExistenceCriterion::Fraction(p) => {
                    let declared = self.declared_size(ordinal);
                    declared > 0 && (masked_count as f64) >= p * (declared as f64)
                }
            };
            if !clears {
                return ArtifactVerdict::Absent(Withheld::Criterion);
            }
        }

        // Last: the first content whose generating set this viewer holds entirely, or nothing.
        let rank = match self.containment(ordinal) {
            Containment::NothingToContain => None,
            Containment::Satisfied(i) => Some(i),
            Containment::Unsatisfied => return ArtifactVerdict::Absent(Withheld::Containment),
        };

        ArtifactVerdict::Serve { masked_count, rank }
    }

    /// Containment, by the partition where there is one and by the mask where there is not.
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

    /// An artifact-major level intersects its own membership with the composed mask; a row-major
    /// level has no per-artifact membership, and takes the count from the per-session histogram.
    fn masked_count(&self, ordinal: u32) -> u64 {
        if self.rows.layout().is_row_major() {
            if let Some(counts) = &self.counts {
                return counts.get(ordinal);
            }
        }
        self.rows.masked_count(ordinal, self.mask)
    }

    /// The proportional criterion's denominator, in row terms so numerator and denominator come
    /// from the same projection: a member still in an unfolded flush extent contributes to
    /// neither, understating rather than overstating the ratio.
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
    use crate::artifacts::test_support::*;
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
    /// served with the same count that was tested.
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

        // One fewer visible member: absent, not served with a rounded count.
        let below = Fixture::new(&[&[1, 2, 3, 4, 5, 6, 7, 8, 9, 10]], &[1, 2, 3, 4]);
        assert_eq!(
            below.view(&d, true).verdict(EntityId::new(999), 0, None),
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
        assert!(view.verdict(EntityId::new(999), 0, None).is_served());
        assert_eq!(
            view.verdict(EntityId::new(998), 1, None),
            ArtifactVerdict::Absent(Withheld::Criterion)
        );
    }

    /// The overlay is first and unconditional: a suppressed artifact is absent regardless.
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

        let mut fx = Fixture::new(&[&[1, 2, 3]], &[1, 2, 3]);
        fx.overlay.apply(entity, ChangeOp::Delete);
        assert_eq!(
            fx.view(&d, true).verdict(entity, 0, None),
            ArtifactVerdict::Absent(Withheld::Verdict)
        );
    }

    /// The own-terms flag and the criterion are independent declarations composed by conjunction:
    /// satisfying one does not switch the other off.
    #[test]
    fn the_own_terms_flag_does_not_disable_the_criterion() {
        let d = declaration(true, Some(ExistenceCriterion::Count(5)));
        let mut fx = Fixture::new(&[&[1, 2, 3, 4, 5, 6, 7, 8, 9, 10]], &[1, 2, 3, 4]);
        fx.satisfied.insert(TermId::new(7));

        assert_eq!(
            fx.view(&d, true)
                .verdict(EntityId::new(999), 0, Some(TermId::new(7))),
            ArtifactVerdict::Absent(Withheld::Criterion)
        );
    }

    /// A layer whose artifacts carry their own terms withholds one that carries none.
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

        // Holds three of four, and a larger visible set overall: what decides is which documents.
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

        for mask in [Bitmap::of(&[1, 2, 3, 4]), Bitmap::of(&[1, 2, 5, 6])] {
            match verdict(&mask) {
                ArtifactVerdict::Serve { rank, .. } => assert_eq!(rank, Some(1)),
                other => panic!("expected the narrow content, got {other:?}"),
            }
        }
        match verdict(&Bitmap::of(&[1, 2, 3, 4, 5, 6])) {
            ArtifactVerdict::Serve { rank, .. } => assert_eq!(rank, Some(0)),
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

    /// A layer declaring no supplied content has nothing to contain.
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
}
