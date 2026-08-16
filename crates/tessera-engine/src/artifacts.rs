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
//! 3. **What it is attached to, if it is an attachment.** An artifact that exists only as an
//!    attachment to another — a toponymy label on a cluster — is tested on its target's disposition
//!    and reachability as well as on its own. Without that term the predicate is per-artifact by
//!    construction, so suppressing a cluster stops the cluster serving while every label naming and
//!    describing it goes on serving to whoever reaches it directly: by search, by a held identifier,
//!    by a filter. The model's conjunctive rule covers edge *traversal* and those routes traverse
//!    nothing (`annotation-representation.md` §4).
//!
//!    **Three terms and not the target's whole predicate**, which is what §4 asks for and where its
//!    cost argument comes from — disposition is the lookup branch 1 already performs and existence
//!    is one store probe, while the target's criterion and containment would need the target level's
//!    projection on every request. The residue is that a target withheld by *its own* criterion can
//!    still be named by a label on a layer that declares a weaker one:
//!    [decision 0086](../../../docs/decisions/0086-the-attachment-term-does-not-inherit-the-targets-criterion.md)
//!    rules that it stays that way, because a label is an artifact with its own declaration and the
//!    surface is C1's r43 one — several layers over one corpus are governed by the most permissive
//!    declaration among them.
//!
//!    **Existence is a separate term from disposition because of the fold.** An overlay entry says
//!    *deleted*; the fold that executes that deletion retires the entry in the same publication that
//!    drops the target's slot, so a term resting on the overlay alone would start serving every
//!    label attached to a deleted cluster at the next nightly fold.
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
use tessera_lifecycle::Overlay;
use tessera_types::layer::{ExistenceCriterion, LayerDeclaration};
use tessera_types::{EntityId, TermId};

use tessera_store::permutation::RowSpace;

use crate::compose::MaskedSet;

/// One layer's membership in the row space of one slice, built at open and rebuilt when the
/// generation moves.
///
/// **Built member-wise, and this is a disclosure rule.** Projecting an entity *range* to a row
/// range would admit whatever documents happen to sit between two members in Morton order — and one
/// extra member can lift an artifact over its existence criterion. The write cycle forbids
/// range-wise translation for that reason; the same rule reaches the build of this form.
#[derive(Debug, Clone, Default)]
pub struct ArtifactRows {
    /// Parallel to a level's ordinals; `None` is a hole, not an empty membership.
    rows: Vec<Option<Bitmap>>,
    /// Per ordinal, per ranked variation: that variation's **generating set** in row space, beside
    /// the size it had in entity space.
    ///
    /// **Both, because a projection that lost a member must not read as containment.** A generating
    /// set is entity-space and permanent; row space holds only what this slice has folded in, so a
    /// member awaiting a fold projects to nothing and would silently drop out of the test — leaving
    /// a viewer contained in a *smaller* set than the caller declared, which is the whole
    /// disclosure. Carrying the declared size makes the loss detectable, and a lossy projection
    /// fails containment for everybody rather than passing it for somebody.
    variations: Vec<Vec<ProjectedSet>>,
    /// Per ordinal, what this artifact is an attachment to — `None` for an ordinary artifact.
    ///
    /// **Entity space, and deliberately not projected.** A target is tested on its disposition and
    /// on its layer's gate, neither of which is a row-space question, so a projection would be a
    /// second address for something already addressed. It rides here because this is the structure a
    /// level's serving path already holds per ordinal.
    attachments: Vec<Option<Attachment>>,
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
    /// The viewer contains this variation's generating set entirely, and is served it whole.
    Satisfied(u32),
    /// The viewer contains no variation's generating set.
    Unsatisfied,
}

/// One variation's generating set, projected, with what it should have projected to.
#[derive(Debug, Clone)]
struct ProjectedSet {
    rows: Bitmap,
    /// `|G|` in entity space, from the durable record.
    declared: u64,
}

impl ArtifactRows {
    /// Project a level's memberships into `space`.
    ///
    /// Costly by design and not on any per-request path: `RowSpace::project` decodes the whole
    /// membership. It is paid at open and at a generation move, which is the same cadence the
    /// session's own mask projection is paid at.
    pub fn build<'a>(
        artifacts: impl Iterator<Item = (u32, &'a ArtifactRecord)>,
        space: &RowSpace,
    ) -> Self {
        let mut rows: Vec<Option<Bitmap>> = Vec::new();
        let mut variations: Vec<Vec<ProjectedSet>> = Vec::new();
        let mut attachments: Vec<Option<Attachment>> = Vec::new();
        for (ordinal, record) in artifacts {
            let idx = ordinal as usize;
            if rows.len() <= idx {
                rows.resize_with(idx + 1, || None);
                variations.resize_with(idx + 1, Vec::new);
                attachments.resize_with(idx + 1, || None);
            }
            rows[idx] = Some(space.project_base(&record.members));
            attachments[idx] = record.attached_to.clone();
            variations[idx] = record
                .variations
                .iter()
                .map(|v| ProjectedSet {
                    // Base rows here too, and here the consequence is sharper than a low count: a
                    // generating set that lost members in projection can never be contained, so a
                    // label whose sample includes documents ingested since the last fold is
                    // withheld from **everyone** until that fold. Fail-closed, and the direction
                    // this must fail in — the alternative is serving content on a set that no
                    // longer names what the text was derived from.
                    rows: space.project_base(&v.generated_from),
                    declared: v.generated_from.cardinality(),
                })
                .collect();
        }
        ArtifactRows {
            rows,
            variations,
            attachments,
        }
    }

    /// What the artifact at `ordinal` hangs from, if it hangs from anything.
    fn attachment(&self, ordinal: u32) -> Option<&Attachment> {
        self.attachments
            .get(ordinal as usize)
            .and_then(Option::as_ref)
    }

    pub fn get(&self, ordinal: u32) -> Option<&Bitmap> {
        self.rows.get(ordinal as usize).and_then(Option::as_ref)
    }

    /// How many ordinals this level covers, holes included.
    pub fn len(&self) -> usize {
        self.rows.len()
    }

    pub fn is_empty(&self) -> bool {
        self.rows.is_empty()
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

    /// The first variation this viewer is served — the containment test
    /// (`annotations.md` §4).
    ///
    /// `Ok(None)` where the artifact carries no variations at all, which is every artifact on a
    /// layer declaring no supplied content: there is nothing to contain, and the artifact serves on
    /// its other conjuncts alone. `Err(())` where it carries variations and the viewer satisfies
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
    pub fn satisfied_variation(&self, ordinal: u32, mask: &impl MaskedSet) -> Containment {
        let Some(sets) = self.variations.get(ordinal as usize) else {
            return Containment::NothingToContain;
        };
        if sets.is_empty() {
            return Containment::NothingToContain;
        }
        for (i, set) in sets.iter().enumerate() {
            // A set that lost members in projection can never be contained — see `ProjectedSet`.
            // Checked before the mask rather than after, because it is a property of the artifact
            // and not of the viewer, and because it must not be expressible as *contained*.
            if set.rows.cardinality() != set.declared {
                continue;
            }
            if mask.count_intersection(&set.rows) == set.declared {
                return Containment::Satisfied(i as u32);
            }
        }
        Containment::Unsatisfied
    }
}

/// What a cached [`ArtifactRows`] was built from. **Every term is a reason the projection would be
/// wrong**, and a mismatch on any of them rebuilds:
///
/// - the **prefix**, because a fold renumbers the base row space wholesale, so a projection built
///   over the old one names other people's documents;
/// - the **slice**, because row space is per slice;
/// - the **store version**, because a publication adds memberships the projection has never seen —
///   and a cached projection that silently omitted them would serve a level with its newest
///   clusters absent, indistinguishable from clusters that failed their criterion.
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
    slice: String,
    store_version: u64,
}

/// One row-space projection per `(slice, layer, level)`, rebuilt when its [`ProjectionKey`] moves.
///
/// **Costly to build and therefore never built on a request that can reuse one.**
/// `RowSpace::project` decodes a whole membership; at corpus scale that is the "seconds, not
/// milliseconds" cost `RowProjection` carries the same warning about. This is paid at the first
/// request after a generation move or a publication, and by nothing else.
///
/// **Replace-on-mismatch, not an LRU.** The key names the only generation a projection is valid
/// for, so a stale entry has no value to retain — keeping one would be keeping a wrong answer
/// warm. The map is therefore bounded by the number of live `(slice, layer, level)` triples rather
/// than by a capacity anyone has to tune.
/// `(slice, layer, level)` — what one cached projection is *for*, as against the
/// [`ProjectionKey`] that says when it stops being valid.
type LevelAddress = (String, String, u32);

#[derive(Debug, Default)]
pub struct ArtifactProjections {
    cached: Mutex<BTreeMap<LevelAddress, (ProjectionKey, Arc<ArtifactRows>)>>,
}

impl ArtifactProjections {
    pub fn new() -> Self {
        Self::default()
    }

    /// This level's row form for the given generation, building it if what is held is stale.
    ///
    /// **The build runs outside the lock**, so a slow projection does not block every other layer's
    /// requests behind it. Two threads racing the same key both build and the last one wins; they
    /// build from the same store version over the same row space, so the two results are equal and
    /// the waste is one projection, not a wrong answer.
    #[allow(clippy::too_many_arguments)]
    pub fn get_or_build(
        &self,
        prefix: &str,
        slice: &str,
        layer: &str,
        level: u32,
        store: &ArtifactStore,
        store_version: u64,
        space: &RowSpace,
    ) -> Arc<ArtifactRows> {
        let key = ProjectionKey {
            prefix: prefix.to_string(),
            slice: slice.to_string(),
            store_version,
        };
        let map_key = (slice.to_string(), layer.to_string(), level);

        if let Some((held, rows)) = self.cached.lock().unwrap_or_else(|e| e.into_inner()).get(&map_key)
        {
            if *held == key {
                return Arc::clone(rows);
            }
        }

        let rows = Arc::new(ArtifactRows::build(store.level(layer, level), space));
        self.cached
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .insert(map_key, (key, Arc::clone(&rows)));
        rows
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
    /// The artifact carries supplied content and this viewer contains no variation's generating
    /// set — or the variation they would have been served has no readable content.
    Containment,
}

/// The outcome of the one predicate.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ArtifactVerdict {
    /// Served, with this masked count beside it, and — where the layer declares supplied content —
    /// the index of the one variation this viewer gets, **entire**.
    Serve {
        masked_count: u64,
        variation: Option<u32>,
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
    /// codebase has made once already, in the slice gate.
    pub satisfied: &'a FxHashSet<TermId>,
    /// Whether the viewer reaches the layer at all. Resolved once per session by the registry, and
    /// passed in rather than recomputed — but see [`ArtifactView::verdict`]: the *overlay* half is
    /// never cached, only this.
    pub layer_reachable: bool,
    /// This slice's row form of the layer's membership.
    pub rows: &'a ArtifactRows,
    /// The gate half of the attachment term: the target layer's own entity where this viewer
    /// reaches that layer, and `None` where they do not.
    ///
    /// **A hook rather than a resolved answer, because the target is another layer.** The layer this
    /// view is for was resolved once per session; a label's target may live in any layer its own
    /// declares in `depends_on`, and which of those a viewer reaches is the same one set probe
    /// `ResolvedLayers` answers for its own. Returning the layer's entity rather than a bool is what
    /// lets the **live** suppression check run here too, in the order it runs for the layer being
    /// served — a cached reachability outliving a layer suppression is the fail-open that ordering
    /// exists to avoid.
    pub attachment_gate: &'a dyn Fn(&str) -> Option<EntityId>,
    /// The existence half of the attachment term: whether the target artifact is still there.
    ///
    /// **Not the same question as the disposition check beside it, and the difference is a fold.**
    /// An overlay entry says *deleted*; the fold that executes that deletion retires the entry in
    /// the same publication that drops the artifact's slot (Rule F's artifact arm, write-path
    /// §5.4). After it, nothing in the overlay says anything about that entity — so a term resting
    /// on disposition alone would start serving every label attached to it, and *a label does not
    /// outlive what it labels* would hold only until the next nightly fold.
    ///
    /// A hole answers `false` here, which is what makes the fold's own hole the durable form of the
    /// withholding rather than a state something has to remember.
    pub attachment_resolves: &'a dyn Fn(&Attachment) -> bool,
    /// The viewer's **composed** mask — see [`MaskedSet`] for why the type forbids anything else.
    pub mask: &'a M,
}

impl<M: MaskedSet> ArtifactView<'_, M> {
    /// The one predicate.
    ///
    /// `own_terms` is the artifact's own access label resolved to a term, or `None` if it carries
    /// none. **A layer that declares `artifacts_carry_own` and an artifact that carries no term is
    /// withheld**, not admitted: the flag says the artifact's existence is gated on its own label,
    /// and an artifact with no label has nothing for a viewer to satisfy. Admitting it would make a
    /// missing declaration a grant to everyone, which is the direction a mistake must never take.
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

        // 3. What it hangs from, if it hangs from anything. Both halves — the target's own
        //    disposition and the reachability of the target's layer — because a label must not
        //    outlive either the existence or the reachability of what it labels. Running it here is
        //    what puts it on every route: search, a held identifier and a filter reach a label
        //    directly and traverse no edge, so a rule stated only for traversal never reaches them.
        if let Some(attachment) = self.rows.attachment(ordinal) {
            let Some(target_layer_entity) = (self.attachment_gate)(&attachment.layer) else {
                return ArtifactVerdict::Absent(Withheld::Attachment);
            };
            for entity in [target_layer_entity, attachment.entity] {
                if self.overlay.is_deleted(entity) || self.overlay.is_suppressed(entity) {
                    return ArtifactVerdict::Absent(Withheld::Attachment);
                }
            }
            // And that the target is still *there*. Once a fold has executed the target's deletion
            // its overlay entry is gone — retired in the same publication that dropped its slot —
            // so the two checks above go quiet on an artifact that no longer exists. This is what
            // carries the withholding past that fold.
            if !(self.attachment_resolves)(attachment) {
                return ArtifactVerdict::Absent(Withheld::Attachment);
            }
        }

        // 4. The artifact's own terms, if its layer says it carries them.
        if self.declaration.access.artifacts_carry_own {
            match own_terms {
                Some(term) if self.satisfied.contains(&term) => {}
                _ => return ArtifactVerdict::Absent(Withheld::OwnTerms),
            }
        }

        // 5. The existence criterion, against the **live** masked count. The same number is
        //    returned to the caller, so the tested quantity and the served quantity cannot drift.
        let masked_count = self.rows.masked_count(ordinal, self.mask);
        if let Some(criterion) = self.declaration.visible_when {
            let clears = match criterion {
                ExistenceCriterion::MinVisible(n) => masked_count >= n,
                // The declared, unmasked size is the denominator — a predicate input the build
                // computes and the test consumes, with no field and no wire shape carrying it (C8).
                // A zero denominator cannot clear a positive fraction, and saying so explicitly
                // avoids a division nobody wants to reason about.
                ExistenceCriterion::MinFraction(p) => {
                    let declared = self.declared_size(ordinal);
                    declared > 0 && (masked_count as f64) >= p * (declared as f64)
                }
            };
            if !clears {
                return ArtifactVerdict::Absent(Withheld::Criterion);
            }
        }

        // 6. Containment, last: the first variation whose generating set this viewer holds
        //    **entirely**. A viewer satisfying none receives no artifact — not the artifact with
        //    its description missing, which is the in-between state decision 0076 forbids.
        let variation = match self.rows.satisfied_variation(ordinal, self.mask) {
            Containment::NothingToContain => None,
            Containment::Satisfied(i) => Some(i),
            Containment::Unsatisfied => return ArtifactVerdict::Absent(Withheld::Containment),
        };

        ArtifactVerdict::Serve {
            masked_count,
            variation,
        }
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
        ContentDeclaration, Hierarchy, HierarchyKind, LayerAccess, MembershipSource,
    };

    fn declaration(carry_own: bool, criterion: Option<ExistenceCriterion>) -> LayerDeclaration {
        LayerDeclaration {
            name: "clusters/a".into(),
            title: "A".into(),
            slices: vec!["s0".into()],
            membership: MembershipSource::Enumerated,
            access: LayerAccess {
                label: None,
                artifacts_carry_own: carry_own,
            },
            visible_when: criterion,
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
            rows: sets.iter().map(|s| Some(Bitmap::of(s))).collect(),
            variations: vec![Vec::new(); sets.len()],
            attachments: vec![None; sets.len()],
        }
    }

    /// The gate an unattached artifact's test passes: nothing calls it, and a `None` if anything did
    /// is the fail-closed answer rather than a pass nobody wrote.
    fn no_targets(_layer: &str) -> Option<EntityId> {
        None
    }

    /// The existence half where the target is still there — the ordinary state, so that a case
    /// about disposition or reachability is not silently answered by this half instead.
    fn target_present(_attachment: &Attachment) -> bool {
        true
    }

    /// The existence half after a fold has retired the target: its slot is a hole, and no overlay
    /// entry survives to say why.
    fn target_gone(_attachment: &Attachment) -> bool {
        false
    }

    /// One artifact, with ranked variations given as `(generating set, declared size)` — the
    /// declared size separate so a test can build the *lossy projection* case, where row space
    /// holds fewer members than the entity-space set the caller published.
    fn rows_with_variations(members: &[u32], variations: &[(&[u32], u64)]) -> ArtifactRows {
        ArtifactRows {
            rows: vec![Some(Bitmap::of(members))],
            variations: vec![variations
                .iter()
                .map(|(set, declared)| ProjectedSet {
                    rows: Bitmap::of(set),
                    declared: *declared,
                })
                .collect()],
            attachments: vec![None],
        }
    }

    struct Fixture {
        overlay: Overlay,
        satisfied: FxHashSet<TermId>,
        rows: ArtifactRows,
        mask: Bitmap,
    }

    impl Fixture {
        fn new(members: &[&[u32]], mask: &[u32]) -> Self {
            Fixture {
                overlay: Overlay::new(),
                satisfied: FxHashSet::default(),
                rows: rows_of(members),
                mask: Bitmap::of(mask),
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
                attachment_gate: &no_targets,
                attachment_resolves: &target_present,
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
        let d = declaration(false, Some(ExistenceCriterion::MinVisible(5)));
        let fx = Fixture::new(&[&[1, 2, 3, 4, 5, 6, 7, 8, 9, 10]], &[1, 2, 3, 4, 5, 6]);
        assert_eq!(
            fx.view(&d, true).verdict(EntityId::new(999), 0, None),
            ArtifactVerdict::Serve {
                masked_count: 6,
                variation: None
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
        let d = declaration(false, Some(ExistenceCriterion::MinFraction(0.5)));

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
        let d = declaration(true, Some(ExistenceCriterion::MinVisible(5)));
        let mut fx = Fixture::new(&[&[1, 2, 3, 4, 5, 6, 7, 8, 9, 10]], &[1, 2, 3, 4]);
        fx.satisfied.insert(TermId::new(7));

        // Terms satisfied, criterion not: still absent. Under the old gate modes this artifact
        // would have been served, with an exact masked count of 4.
        assert_eq!(
            fx.view(&d, true).verdict(EntityId::new(999), 0, Some(TermId::new(7))),
            ArtifactVerdict::Absent(Withheld::Criterion)
        );
    }

    /// A layer declaring that its artifacts carry their own terms, and an artifact carrying none,
    /// is withheld. Admitting it would make a missing declaration a grant to everyone.
    #[test]
    fn an_artifact_with_no_terms_on_a_carry_own_layer_is_withheld() {
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
        let rows = rows_with_variations(&[1, 2, 3, 10, 11, 12, 13], &[(generating, 4)]);

        // Holds all four: served, and told which variation.
        let all = Bitmap::of(&[1, 2, 3, 10, 11, 12, 13]);
        let view = ArtifactView {
            declaration: &d,
            overlay: &Overlay::new(),
            satisfied: &FxHashSet::default(),
            layer_reachable: true,
            rows: &rows,
            mask: &all,
            attachment_gate: &no_targets,
                attachment_resolves: &target_present,
        };
        assert_eq!(
            view.verdict(EntityId::new(999), 0, None),
            ArtifactVerdict::Serve {
                masked_count: 7,
                variation: Some(0)
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
            attachment_gate: &no_targets,
                attachment_resolves: &target_present,
        };
        assert_eq!(
            view.verdict(EntityId::new(999), 0, None),
            ArtifactVerdict::Absent(Withheld::Containment)
        );
    }

    /// **The ranking is the caller's and the service takes no opinion on it** (decision 0078): the
    /// first variation the viewer contains is the one they get, entire. This is the worked example
    /// the design turns on — a broad viewer and a narrow viewer failing the *same* full-sample
    /// label, and both satisfying a narrower one.
    #[test]
    fn the_first_variation_the_viewer_contains_is_the_one_they_get() {
        let d = declaration(false, None);
        // Variation 0 was generated from the whole sample; variation 1 from a single term's worth.
        let rows = rows_with_variations(
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
                attachment_gate: &no_targets,
                attachment_resolves: &target_present,
            }
            .verdict(EntityId::new(999), 0, None)
        };

        // Two viewers with nothing in common beyond the narrow set, and neither holds the whole
        // sample: both fail variation 0 and both are served variation 1.
        for mask in [Bitmap::of(&[1, 2, 3, 4]), Bitmap::of(&[1, 2, 5, 6])] {
            match verdict(&mask) {
                ArtifactVerdict::Serve { variation, .. } => assert_eq!(variation, Some(1)),
                other => panic!("expected the narrow variation, got {other:?}"),
            }
        }
        // And a viewer holding everything gets the caller's first choice rather than the fallback.
        match verdict(&Bitmap::of(&[1, 2, 3, 4, 5, 6])) {
            ArtifactVerdict::Serve { variation, .. } => assert_eq!(variation, Some(0)),
            other => panic!("expected the ranked-first variation, got {other:?}"),
        }
        // A viewer holding none of it receives no artifact — not the count without the label.
        assert_eq!(
            verdict(&Bitmap::of(&[3, 4])),
            ArtifactVerdict::Absent(Withheld::Containment)
        );
    }

    /// **A generating set that lost members in projection fails for everybody.**
    ///
    /// Row space holds what this slice has folded in; a member awaiting a fold projects to nothing.
    /// Testing the projected set alone would let a viewer be contained in a *smaller* set than the
    /// caller declared — containment passing on a set the caller never wrote.
    #[test]
    fn a_generating_set_that_did_not_survive_projection_contains_nobody() {
        let d = declaration(false, None);
        // Two rows survived; the caller published four.
        let rows = rows_with_variations(&[1, 2, 3], &[(&[1, 2], 4)]);
        let everything = Bitmap::from_range(0..1000);
        let view = ArtifactView {
            declaration: &d,
            overlay: &Overlay::new(),
            satisfied: &FxHashSet::default(),
            layer_reachable: true,
            rows: &rows,
            mask: &everything,
            attachment_gate: &no_targets,
                attachment_resolves: &target_present,
        };
        assert_eq!(
            view.verdict(EntityId::new(999), 0, None),
            ArtifactVerdict::Absent(Withheld::Containment),
            "a viewer who can see every row there is must still not be served a set that lost \
             members on the way into row space"
        );
    }

    // ---- the attachment term ------------------------------------------------------------------

    /// The cluster a label hangs from, as these tests address it.
    const CLUSTERS: &str = "clusters/a";
    const CLUSTER_LAYER_ENTITY: EntityId = EntityId::new(4_294_967_290);
    const CLUSTER_ENTITY: EntityId = EntityId::new(4_294_901_760);
    const LABEL_ENTITY: EntityId = EntityId::new(4_294_836_224);

    /// One label, attached to a cluster in another layer.
    fn attached_rows(members: &[u32]) -> ArtifactRows {
        ArtifactRows {
            rows: vec![Some(Bitmap::of(members))],
            variations: vec![Vec::new()],
            attachments: vec![Some(Attachment {
                layer: CLUSTERS.to_string(),
                level: 0,
                ordinal: 3,
                entity: CLUSTER_ENTITY,
            })],
        }
    }

    fn reaches_clusters(layer: &str) -> Option<EntityId> {
        (layer == CLUSTERS).then_some(CLUSTER_LAYER_ENTITY)
    }

    /// **The fail-open this term closes.** Suppressing a cluster hides the cluster; without the
    /// extra term every label naming and describing it goes on serving to anyone holding their
    /// identifier — and those labels *are* the description of the thing that was just hidden.
    #[test]
    fn suppressing_a_cluster_withholds_the_labels_attached_to_it() {
        let d = declaration(false, None);
        let rows = attached_rows(&[1, 2, 3]);
        let mask = Bitmap::of(&[1, 2, 3]);

        let verdict = |overlay: &Overlay| {
            ArtifactView {
                declaration: &d,
                overlay,
                satisfied: &FxHashSet::default(),
                layer_reachable: true,
                rows: &rows,
                mask: &mask,
                attachment_gate: &reaches_clusters,
                attachment_resolves: &target_present,
            }
            .verdict(LABEL_ENTITY, 0, None)
        };

        let mut overlay = Overlay::new();
        assert!(verdict(&overlay).is_served());

        // The label's own entity is untouched; only the cluster's is suppressed.
        overlay.apply(CLUSTER_ENTITY, ChangeOp::Suppress);
        assert_eq!(
            verdict(&overlay),
            ArtifactVerdict::Absent(Withheld::Attachment)
        );

        // And a deletion, which is the irreversible one.
        let mut overlay = Overlay::new();
        overlay.apply(CLUSTER_ENTITY, ChangeOp::Delete);
        assert_eq!(
            verdict(&overlay),
            ArtifactVerdict::Absent(Withheld::Attachment)
        );
    }

    /// **The fold's own fail-open, and the one this term's third half exists for.** A deletion's
    /// overlay entry is retired by the fold that executes it — in the same publication that drops
    /// the target's slot — so after that fold nothing in the overlay says the cluster was ever
    /// deleted. A term resting on disposition alone reads "not deleted, not suppressed" and serves
    /// every label attached to it, which turns *a label does not outlive what it labels* into a rule
    /// that holds until the next nightly maintenance window.
    #[test]
    fn a_label_stays_withheld_after_the_fold_that_retired_its_targets_entry() {
        let d = declaration(false, None);
        let rows = attached_rows(&[1, 2, 3]);
        let mask = Bitmap::of(&[1, 2, 3]);
        // The post-fold state exactly: a clean overlay — the entry retired with the deletion it
        // executed — and a target whose slot is now a hole.
        let overlay = Overlay::new();
        let view = |resolves: &dyn Fn(&Attachment) -> bool| {
            ArtifactView {
                declaration: &d,
                overlay: &overlay,
                satisfied: &FxHashSet::default(),
                layer_reachable: true,
                rows: &rows,
                mask: &mask,
                attachment_gate: &reaches_clusters,
                attachment_resolves: resolves,
            }
            .verdict(LABEL_ENTITY, 0, None)
        };

        assert!(
            view(&target_present).is_served(),
            "the same overlay and the same gate serve the label while its target is there — so \
             what the case below asserts is the existence term and nothing else"
        );
        assert_eq!(
            view(&target_gone),
            ArtifactVerdict::Absent(Withheld::Attachment)
        );
    }

    /// **The gate half, which is not optional**: a label must not outlive the *reachability* of what
    /// it labels, not only its existence. A viewer who cannot reach the cluster layer would
    /// otherwise be told what its clusters are called by a label layer they can reach.
    #[test]
    fn a_label_is_withheld_where_the_target_layer_is_not_reached() {
        let d = declaration(false, None);
        let rows = attached_rows(&[1, 2, 3]);
        let mask = Bitmap::of(&[1, 2, 3]);
        let verdict = |gate: &dyn Fn(&str) -> Option<EntityId>| {
            ArtifactView {
                declaration: &d,
                overlay: &Overlay::new(),
                satisfied: &FxHashSet::default(),
                layer_reachable: true,
                rows: &rows,
                mask: &mask,
                attachment_gate: gate,
                attachment_resolves: &target_present,
            }
            .verdict(LABEL_ENTITY, 0, None)
        };

        assert!(verdict(&reaches_clusters).is_served());
        // Unreachable, dropped, never registered: one answer, because which of them applies is
        // exactly what the gate withholds.
        assert_eq!(
            verdict(&no_targets),
            ArtifactVerdict::Absent(Withheld::Attachment)
        );
    }

    /// Suppressing the *layer* the target lives in withholds the labels too — the live half of the
    /// gate, which a cached reachability would otherwise outlive.
    #[test]
    fn suppressing_the_target_layer_withholds_the_labels_attached_into_it() {
        let d = declaration(false, None);
        let rows = attached_rows(&[1, 2, 3]);
        let mask = Bitmap::of(&[1, 2, 3]);
        let mut overlay = Overlay::new();
        overlay.apply(CLUSTER_LAYER_ENTITY, ChangeOp::Suppress);

        assert_eq!(
            ArtifactView {
                declaration: &d,
                overlay: &overlay,
                satisfied: &FxHashSet::default(),
                layer_reachable: true,
                rows: &rows,
                mask: &mask,
                attachment_gate: &reaches_clusters,
                attachment_resolves: &target_present,
            }
            .verdict(LABEL_ENTITY, 0, None),
            ArtifactVerdict::Absent(Withheld::Attachment)
        );
    }

    /// An artifact that hangs from nothing asks nothing, so a clustering pays no lookup per cluster
    /// for a relationship it does not have.
    #[test]
    fn an_unattached_artifact_never_consults_the_gate() {
        let d = declaration(false, None);
        // `no_targets` refuses every layer; the artifact serves anyway, because nothing asks.
        let fx = Fixture::new(&[&[1, 2, 3]], &[1, 2, 3]);
        assert!(fx
            .view(&d, true)
            .verdict(EntityId::new(999), 0, None)
            .is_served());
    }

    /// A layer declaring no supplied content has nothing to contain, and its artifacts serve on the
    /// other conjuncts alone — which is every artifact Stage 2 could publish.
    #[test]
    fn an_artifact_with_no_variations_has_nothing_to_contain() {
        let d = declaration(false, None);
        let fx = Fixture::new(&[&[1, 2, 3]], &[1, 2, 3]);
        assert_eq!(
            fx.view(&d, true).verdict(EntityId::new(999), 0, None),
            ArtifactVerdict::Serve {
                masked_count: 3,
                variation: None
            }
        );
    }
}
