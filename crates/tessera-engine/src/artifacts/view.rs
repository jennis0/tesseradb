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

    /// The prerequisite where the dependency is not served to this viewer: suppressed, deleted,
    /// gone at a fold, in a layer they do not reach, or simply below its own layer's bar. The
    /// predicate treats them alike, and which of them applies is exactly what is withheld.
    fn dependency_absent(_attachment: &Attachment) -> bool {
        false
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
}
