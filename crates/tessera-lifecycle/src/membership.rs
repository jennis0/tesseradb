//! What an artifact's membership is, and where the canonical copy lives.
//!
//! **Entity space, always, for the durable form.** Entity ids are permanent and view-invariant;
//! row space is per view, derived, and renumbered globally by every fold. A membership stored in
//! row space would be a frozen projection — correct until the first fold, then silently naming
//! other people's documents. The row form is built from this one at open and rebuilt when the
//! generation moves (`annotation-representation.md` §2.1), and it never travels.
//!
//! **The row form is built member-wise and never range-wise**, which is a disclosure rule rather
//! than an implementation note. Translating an entity *range* to a row range would let a Morton
//! neighbour — a document that happens to sit next to a member in row order and belongs to nobody's
//! membership — join the set, and a single extra member can lift an artifact over its existence
//! criterion. The write cycle already forbids range-wise translation for exactly this reason; the
//! same rule reaches the build of the resident form.
//!
//! ## What is not stored, and why the absence is the design
//!
//! **No bounding box.** An earlier draft kept a build-time box over full membership and served an
//! artifact wherever that box intersected the viewport — which discloses the unmasked extent by
//! panning: a viewer sees the edge of a shape in a region holding nothing they may see. Candidacy is
//! a masked question instead, answered from the row form against the viewer's own mask, so no
//! representation here can express the fault.
//!
//! **No unmasked count on any wire shape.** [`ArtifactRecord::declared_size`] is the artifact's
//! full membership size and it exists for exactly one consumer: the proportional existence
//! criterion, which divides by it. It is a **predicate input** — the build computes it, the test
//! consumes it, and nothing serialises it to a client — because a corpus-wide count over items a
//! principal may not see is C8's row, one careless line from being served beside a masked one.

use std::collections::BTreeMap;

use croaring::{Bitmap, Portable};
use tessera_types::layer::OnMemberDeletion;
use tessera_types::EntityId;

/// Execute a layer's `on_member_deletion` declaration against one record, for the members this fold
/// retired (`annotation-write-cycle.md` §3.2).
///
/// **An artifact left with no variations is not an artifact with no content** — it is one the
/// serving path withholds, because its layer declares supplied content and it has none to serve.
/// That is [decision 0076](../../../docs/decisions/0076-an-artifact-is-served-whole-or-not-at-all.md)
/// reached from the write side: the alternative is serving the identity and the count with the
/// description missing, which is the in-between state the decision forbids.
fn apply_deletion_policy(record: &mut ArtifactRecord, retired: &Bitmap, policy: OnMemberDeletion) {
    match policy {
        OnMemberDeletion::WithdrawContent => {
            record
                .variations
                .retain(|variation| variation.generated_from.and_cardinality(retired) == 0);
        }
        OnMemberDeletion::ShrinkGeneratingSet => {
            for variation in &mut record.variations {
                variation.generated_from.andnot_inplace(retired);
            }
        }
    }
}

/// One level's not-yet-published artifacts, ready to pack: `(layer, level, ordinal_lo, blobs)`.
///
/// A tuple alias rather than a struct because it is a *transfer* between two modules that both
/// already name these four things — a struct would be a third name for the same tuple, and the
/// packer takes them apart again immediately.
pub type PendingExtent = (String, u32, u32, Vec<Vec<u8>>);

/// One artifact as a caller offers it, before the engine has given it an ordinal or an entity.
///
/// **Members are entities, resolved at admission.** A caller names them by `tessera_id` and the
/// control plane inverts them once, at the boundary, exactly as `/control/changes` does — so no
/// blinded identifier reaches durable state, where a key rotation would silently redirect it (I10).
#[derive(Debug, Clone, PartialEq)]
pub struct IncomingArtifact {
    pub stable_key: Option<String>,
    pub members: Bitmap,
    /// The artifact's supplied content, as **ranked variations** — most specific first. Empty on a
    /// layer that declares no supplied content, which is every layer Stage 2 could publish.
    ///
    /// A viewer is served the first variation whose generating set they contain, entire, or the
    /// artifact is absent ([decision 0076](../../../docs/decisions/0076-an-artifact-is-served-whole-or-not-at-all.md)).
    /// The order is the caller's ranking and the service takes no opinion on it
    /// ([decision 0078](../../../docs/decisions/0078-the-service-takes-no-opinion-on-which-variation.md)).
    pub variations: Vec<IncomingVariation>,
    /// The artifact this one exists only as an attachment to — a toponymy label on a cluster.
    ///
    /// **Named by the target's own stable key, because an ordinal is never disclosed.** A response
    /// carries a `tessera_id` and never a position in a dense level (C8), so the caller holds no
    /// address for the target beyond the key they published it under.
    pub attached_to: Option<IncomingAttachment>,
    /// The parent artifact in a hierarchical layer, named by stable key (stage 5).
    pub parent_key: Option<String>,
    /// The child artifacts in a hierarchical layer, named by stable keys (stage 5).
    pub children_keys: Vec<String>,
}

/// The target of an attachment, as a caller names it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct IncomingAttachment {
    pub layer: String,
    pub level: u32,
    pub stable_key: String,
}

/// One ranked variation of an artifact's supplied content, as a caller offers it.
#[derive(Debug, Clone, PartialEq)]
pub struct IncomingVariation {
    /// One value per kind the layer declares, **positionally**. Every declared kind must be
    /// supplied: `/v1/meta` publishes the kinds so a client knows what to draw, and that is only
    /// safe because no served artifact ever lacks one its layer declared.
    pub values: Vec<String>,
    /// The documents this content was generated from — the set a viewer must contain **entirely**
    /// to be served it.
    ///
    /// Empty exactly where the layer declares no corpus-derived kind, in which case containment is
    /// vacuous and the content serves unconditionally. A set supplied where none is tested is
    /// refused rather than stored: a claim the service carries and never checks is worse than no
    /// claim, because a reader takes its presence for a control.
    pub generated_from: Bitmap,
}

impl IncomingVariation {
    /// Builds one from resolved entities — the constructor exists for
    /// [`IncomingArtifact::from_entities`]'s reason: `tessera-server` names a set without being
    /// able to do arithmetic on one.
    pub fn new(values: Vec<String>, generated_from: impl IntoIterator<Item = EntityId>) -> Self {
        let mut bitmap = Bitmap::new();
        for entity in generated_from {
            bitmap.add(entity.raw() as u32);
        }
        IncomingVariation {
            values,
            generated_from: bitmap,
        }
    }
}

impl IncomingArtifact {
    /// Builds one from resolved entities.
    ///
    /// **The constructor exists so the bitmap type stays inside this crate.** `tessera-server`
    /// assembles these from a resolved batch and carries no Roaring dependency — a layering
    /// `check-layers.sh` holds, and one worth holding: the request plane should be able to name a
    /// membership without being able to do arithmetic on one.
    pub fn from_entities(
        stable_key: Option<String>,
        members: impl IntoIterator<Item = EntityId>,
    ) -> Self {
        let mut bitmap = Bitmap::new();
        for entity in members {
            // Entity space is `u32` by I9, so the narrowing is total.
            bitmap.add(entity.raw() as u32);
        }
        IncomingArtifact {
            stable_key,
            members: bitmap,
            variations: Vec::new(),
            attached_to: None,
            parent_key: None,
            children_keys: Vec::new(),
        }
    }

    /// The same, attached to another layer's artifact — the shape a label layer publishes.
    pub fn attached(
        stable_key: Option<String>,
        members: impl IntoIterator<Item = EntityId>,
        variations: Vec<IncomingVariation>,
        attached_to: IncomingAttachment,
    ) -> Self {
        let mut artifact = IncomingArtifact::with_content(stable_key, members, variations);
        artifact.attached_to = Some(attached_to);
        artifact
    }

    /// The same, carrying supplied content.
    pub fn with_content(
        stable_key: Option<String>,
        members: impl IntoIterator<Item = EntityId>,
        variations: Vec<IncomingVariation>,
    ) -> Self {
        let mut artifact = IncomingArtifact::from_entities(stable_key, members);
        artifact.variations = variations;
        artifact
    }
}

/// What one fold's deletions took from one artifact — a row of the fold's report.
///
/// **Addressed by the caller's own key where they supplied one**, because that is the name they can
/// act on: a `tessera_id` is what a *viewer* holds, and the ordinal is an internal address that no
/// response carries. A caller who published without a key gets the address and can still find the
/// artifact by it on the control plane.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Degradation {
    pub layer: String,
    pub level: u32,
    pub ordinal: u32,
    pub stable_key: Option<String>,
    /// How many of this artifact's members the fold retired. Zero where only its content lost
    /// sources — the two losses are independent.
    pub members_lost: u64,
    /// What the membership held before this fold, so a caller can see the proportion rather than
    /// having to hold the previous number themselves.
    pub declared_members: u64,
    /// `(variation index, members of that generating set the fold retired)`, for the variations
    /// that lost any. Empty on a layer that declares no supplied content.
    pub variations_lost: Vec<(u32, u64)>,
}

/// One artifact's durable state, as the registry holds it.
#[derive(Debug, Clone)]
pub struct ArtifactRecord {
    /// This artifact's own entity — its address for the deny lane, and what `tessera_id` blinds.
    pub entity: EntityId,
    /// The caller's own key, if they supplied one. **Effectively mandatory for a layer another
    /// layer's edges point into**: an edge names its target, and at publish time the caller holds
    /// no `tessera_id` for it.
    pub stable_key: Option<String>,
    /// Entity-space membership — the canonical, view-invariant record.
    pub members: Bitmap,
    /// The ranked variations of this artifact's supplied content, most specific first.
    ///
    /// **The values are not here.** This carries each variation's *generating set* — the thing the
    /// serving path does bitmap arithmetic on for every request — while the content bytes live in
    /// the record blob at this artifact's entity
    /// ([decision 0077](../../../docs/decisions/0077-supplied-content-lives-in-the-record-blob.md)).
    /// The split follows from what each is for: a generating set is projected into row space once
    /// per generation and intersected per request, and a form that had to be decompressed to be
    /// tested would pay that cost on every artifact of every viewport.
    pub variations: Vec<VariationSet>,
    /// What this artifact exists as an attachment to, resolved at publication.
    ///
    /// **A visibility term, not a navigation aid.** An artifact carrying one is tested on its
    /// target's disposition and reachability as well as on its own conjuncts, on **every** route —
    /// see [`Attachment`].
    pub attached_to: Option<Attachment>,
    /// This artifact's parent in its layer's hierarchy.
    ///
    /// **The opposite of an attachment in the one way that matters**: it is *not* a visibility term.
    /// A node's verdict is its own masked count against its own criterion, with no input from its
    /// lineage and none from the viewport
    /// ([decision 0080](../../../docs/decisions/0080-the-frontier-is-a-per-artifact-test.md)) — so
    /// a parent that is suppressed, deleted or below its bar withholds itself and nothing else. What
    /// the edge decides is only which of two artifacts that *both* passed is the one drawn.
    ///
    /// A parent whose ordinal is now a hole leaves this node a root, which serves it: correct, since
    /// it passed its own test, and the reason the fold does not have to rewrite these.
    ///
    /// **The level is carried because a layer's edges are one of two shapes.** A nested layer's run
    /// within one level, and the cut climbs them; a tiered layer's run between levels, and
    /// the cut does not — those are information about what contains what, not a ladder to coarsen
    /// along (owner ruling, 2026-08-18).
    pub parent: Option<crate::wal::ParentRef>,
}

/// The resolved target of an attachment: the edge `annotation-representation.md` §2.4 names, with
/// the target's entity carried beside it.
///
/// **The entity is stored rather than re-derived, because it is what the predicate reads.** The
/// extra term an attached artifact carries is one `verdict` lookup — the same lookup the
/// predicate's first branch already performs on the artifact's own entity — and re-deriving it from
/// the target layer's reserved runs on every request would put the registry in a path that needs
/// nothing but an identifier. The address `(layer, level, ordinal)` travels with it because that is
/// the edge's identity, and traversal will read it.
///
/// **The target exists before the edge does** (§5.0.4). An edge names a position in a dense level,
/// so one written ahead of its target would name whatever later landed there; publication refuses
/// an unresolvable target rather than storing one. The target's layer must be one the attaching
/// layer declared in `depends_on`, which is what makes the layer-level refusal of a dangling
/// replacement sound: a dependency nobody declared is one nothing checks.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Attachment {
    pub layer: String,
    pub level: u32,
    pub ordinal: u32,
    /// The target's own entity — its address for the deny lane, and the identifier the extra
    /// predicate term is evaluated on.
    pub entity: EntityId,
}

/// One variation's generating set, as the registry holds it, and the content it gates.
#[derive(Debug, Clone)]
pub struct VariationSet {
    /// The content values, positional to the layer's declared kinds.
    ///
    /// **`None` where this copy does not carry them**, which means *restored from a packed extent
    /// rather than replayed from the log*: the extent carries generating sets and not values,
    /// because values live in the record blob
    /// ([decision 0077](../../../docs/decisions/0077-supplied-content-lives-in-the-record-blob.md)).
    ///
    /// It is not an absence of content — the serving path reads the blob at the artifact's own
    /// entity when this is `None`, and the two copies are written by the same publication. What it
    /// means is *ask the blob*, and a blob that cannot answer **withholds the artifact**: served
    /// with its identity and its count and no description is the in-between state decision 0076
    /// forbids.
    pub values: Option<Vec<String>>,
    /// Entity-space, canonical. **Empty means corpus-independent** — containment is vacuous and the
    /// variation serves to everyone who reaches the layer — and that is a real declaration rather
    /// than a missing one: a layer whose kinds are all corpus-independent is refused a generating
    /// set at publish, so an empty set here cannot be an omission.
    pub generated_from: Bitmap,
}

impl ArtifactRecord {
    /// The artifact's **declared** membership size: how many members it was published with,
    /// unmasked.
    ///
    /// **A predicate input and never a field.** The proportional criterion divides by it; nothing
    /// else may read it, and nothing serialises it. Deriving it from `members` rather than storing
    /// it separately is deliberate — a stored copy is a number someone can reach for, and this one
    /// cannot drift from the set it describes.
    pub fn declared_size(&self) -> u64 {
        self.members.cardinality()
    }
}

/// Every artifact of every layer, keyed by `(layer, level, ordinal)`.
///
/// **Ordinals are dense within a level**, so a level is a vector rather than a map: the address is
/// `entity − run.start` arithmetic, and an ordinal that names no artifact is a hole rather than a
/// lookup miss. A hole is a real state — an artifact whose publication is still in flight, or one
/// a fold has yet to remove — and it must answer *absent* rather than panic.
#[derive(Debug, Clone, Default)]
pub struct ArtifactStore {
    levels: BTreeMap<(String, u32), Vec<Option<ArtifactRecord>>>,
    /// `(layer, level, stable_key) → ordinal`. **An index, not a second copy of the truth**: it
    /// exists so a batch of ten thousand artifacts can be checked for duplicate keys in
    /// `O(n log n)` rather than rescanning the level per artifact, which is `O(n²)` and reachable
    /// at the sizes this stage publishes.
    keys: BTreeMap<(String, u32, String), u32>,
    /// Where the oldest surviving publication sits in the log — the bound rotation may not reclaim
    /// past. See [`ArtifactStore::oldest_wal_pos`].
    oldest_wal_pos: Option<u64>,
    /// Per `(layer, level)`, the ordinal high-water already durable in a manifest. Everything at or
    /// above it lives only in the WAL, which is what the rotation pin holds the log for.
    published_through: BTreeMap<(String, u32), u32>,
    /// Bumped by every publication and every layer removal. **A derived row-space projection is
    /// valid only for the version it was built from**: a cache that missed a bump would serve a
    /// level with its newest artifacts absent, which a viewer cannot tell from artifacts that
    /// failed their existence criterion.
    version: u64,
}

impl ArtifactStore {
    pub fn new() -> Self {
        Self::default()
    }

    /// Insert or replace one artifact. Growing the level's vector to fit is what makes a
    /// publication that arrives out of ordinal order land correctly.
    pub fn put(&mut self, layer: &str, level: u32, ordinal: u32, record: ArtifactRecord) {
        if let Some(key) = &record.stable_key {
            self.keys
                .insert((layer.to_string(), level, key.clone()), ordinal);
        }
        let slots = self
            .levels
            .entry((layer.to_string(), level))
            .or_default();
        let idx = ordinal as usize;
        if slots.len() <= idx {
            slots.resize(idx + 1, None);
        }
        slots[idx] = Some(record);
    }

    /// The next ordinal a publication into this level would claim.
    ///
    /// **Derived from the level's extent, never stored**, so replay reconstructs it exactly rather
    /// than needing a durable cursor. A hole left by a removal is *not* reused: the entity behind
    /// it is not reclaimed either (decision 0072 is settled and unbuilt), and handing the ordinal
    /// back while the entity stays spent is how the two would come to disagree.
    pub fn next_ordinal(&self, layer: &str, level: u32) -> u32 {
        self.levels
            .get(&(layer.to_string(), level))
            .map(|slots| slots.len() as u32)
            .unwrap_or(0)
    }

    /// The ordinal a caller's own key names in this level, if any.
    pub fn ordinal_of_key(&self, layer: &str, level: u32, key: &str) -> Option<u32> {
        self.keys
            .get(&(layer.to_string(), level, key.to_string()))
            .copied()
    }

    /// The log position of the oldest surviving publication, or `None` if none survives.
    ///
    /// **Rotation may not reclaim past this, and today that pins the log from the first
    /// publication onwards.** Nothing but the WAL carries a membership: a manifest carries the
    /// registry, segments carry rows and postings, and neither carries a Roaring bitmap of who
    /// belongs to a cluster. So reclaiming a member holding an `ArtifactPublish` destroys the only
    /// copy — a served cluster that comes back from a restart with no members, which the existence
    /// criterion then renders as *absent* rather than as an error.
    ///
    /// ⊘ **This is an open question answered fail-closed, not a design.** Where membership lives on
    /// disk is the owner's decision and the one layout question decisions 0074–0081 left open;
    /// until it lands, an unbounded log is the safe direction and a visible one. It is the same
    /// posture `oldest_wal_pos`'s unknown-position arm takes in the ingest buffer, and for the same
    /// reason: a sequence that grows is noticed, a record that vanishes is not.
    pub fn oldest_wal_pos(&self) -> Option<u64> {
        self.oldest_wal_pos
    }

    /// Applies a durable publication — the one path by which memberships enter, taken by both the
    /// live write path and replay.
    ///
    /// `position` is where the record sits in the log. **Replay applies the recorded ordinals and
    /// entities rather than re-deriving them**, on [`crate::LayerRegistry::apply`]'s contract: a
    /// re-derived ordinal would move an artifact under every suppression naming it.
    ///
    /// Records other than a publication are ignored, so a caller can hand the whole replay stream
    /// to this and to the registry alike.
    ///
    /// Returns how many memberships **did not decode** — always zero in any healthy log. The count
    /// is returned rather than logged because this crate carries no tracing dependency by design
    /// (see `check-layers.sh`), and a silent skip is the one outcome this must not have: an
    /// artifact whose members were lost is served as absent, which is indistinguishable from one
    /// that never cleared its criterion.
    #[must_use]
    pub fn apply(&mut self, record: &crate::wal::WalRecord, position: u64) -> usize {
        let crate::wal::WalRecord::ArtifactPublish {
            layer,
            level,
            artifacts,
            ..
        } = record
        else {
            return 0;
        };
        let mut refused = 0;
        for published in artifacts {
            // Damage is a refusal, not an empty membership — see `deserialise_members`. Skipping
            // leaves a hole, which answers *absent*; the alternative decodes a corrupt record to a
            // legitimately emptied artifact and serves it.
            let Some(members) = deserialise_members(&published.members) else {
                refused += 1;
                continue;
            };
            // Every generating set decodes or the artifact is refused whole. A variation whose set
            // decoded short is one a viewer may be served without containing what it was generated
            // from — the disclosure containment exists to prevent — so the failure may not be
            // localised to the variation and skipped.
            let sets: Option<Vec<VariationSet>> = published
                .variations
                .iter()
                .map(|v| {
                    deserialise_members(&v.generated_from).map(|generated_from| VariationSet {
                        values: Some(v.values.clone()),
                        generated_from,
                    })
                })
                .collect();
            let Some(variations) = sets else {
                refused += 1;
                continue;
            };
            self.put(
                layer,
                *level,
                published.ordinal,
                ArtifactRecord {
                    entity: published.entity,
                    stable_key: published.stable_key.clone(),
                    members,
                    variations,
                    attached_to: published.attached_to.clone().map(|a| Attachment {
                        layer: a.layer,
                        level: a.level,
                        ordinal: a.ordinal,
                        entity: a.entity,
                    }),
                    parent: published.parent,
                },
            );
        }
        self.oldest_wal_pos = Some(match self.oldest_wal_pos {
            Some(existing) => existing.min(position),
            None => position,
        });
        self.version += 1;
        refused
    }

    pub fn get(&self, layer: &str, level: u32, ordinal: u32) -> Option<&ArtifactRecord> {
        self.levels
            .get(&(layer.to_string(), level))
            .and_then(|slots| slots.get(ordinal as usize))
            .and_then(Option::as_ref)
    }

    /// Every artifact of one level, with its ordinal. Holes are skipped.
    pub fn level(&self, layer: &str, level: u32) -> impl Iterator<Item = (u32, &ArtifactRecord)> {
        self.levels
            .get(&(layer.to_string(), level))
            .into_iter()
            .flat_map(|slots| {
                slots
                    .iter()
                    .enumerate()
                    .filter_map(|(i, slot)| slot.as_ref().map(|r| (i as u32, r)))
            })
    }

    /// Every artifact of every level of one layer, as `(level, ordinal, record)`.
    pub fn layer<'a>(
        &'a self,
        layer: &'a str,
    ) -> impl Iterator<Item = (u32, u32, &'a ArtifactRecord)> + 'a {
        self.levels
            .range((layer.to_string(), 0)..)
            .take_while(move |((l, _), _)| l == layer)
            .flat_map(|((_, level), slots)| {
                slots
                    .iter()
                    .enumerate()
                    .filter_map(move |(i, slot)| slot.as_ref().map(|r| (*level, i as u32, r)))
            })
    }

    /// Drop every artifact of a layer — what a layer drop leaves behind otherwise.
    ///
    /// **The log pin is not lowered with them.** A rotation that reclaimed back to where this
    /// layer's publications sat would also reclaim every *other* layer's records in between, and
    /// the pin is a single bound rather than a set. Holding it costs a longer log; recomputing it
    /// wrongly costs a membership.
    pub fn remove_layer(&mut self, layer: &str) {
        self.levels.retain(|(l, _), _| l != layer);
        self.keys.retain(|(l, _, _), _| l != layer);
        self.version += 1;
    }

    /// See the field: what a derived projection's validity is keyed on.
    pub fn version(&self) -> u64 {
        self.version
    }

    /// Every level's artifacts that are **not yet in a manifest**, as
    /// `(layer, level, ordinal_lo, blobs)` ready to pack — see [`encode_record`].
    ///
    /// **A level with a hole in its unpublished range is skipped whole and reported**, rather than
    /// packed around: an extent addresses `[ordinal_lo, ordinal_lo + count)` densely, so a hole
    /// would shift every later artifact's identity by one. A hole here means a publication landed
    /// out of order, which nothing does today.
    pub fn unpublished(&self) -> (Vec<PendingExtent>, Vec<(String, u32)>) {
        let mut ready = Vec::new();
        let mut skipped = Vec::new();
        for ((layer, level), slots) in &self.levels {
            let from = *self.published_through.get(&(layer.clone(), *level)).unwrap_or(&0) as usize;
            if from >= slots.len() {
                continue;
            }
            let tail = &slots[from..];
            if tail.iter().any(Option::is_none) {
                skipped.push((layer.clone(), *level));
                continue;
            }
            let blobs: Vec<Vec<u8>> = tail
                .iter()
                .map(|slot| encode_record(slot.as_ref().expect("checked dense just above")))
                .collect();
            ready.push((layer.clone(), *level, from as u32, blobs));
        }
        (ready, skipped)
    }

    /// What this fold's deletions took away from every artifact that held one — the sweep behind
    /// the fold's report (`annotation-write-cycle.md` §4.2).
    ///
    /// **One `and_cardinality` per artifact, against a set the fold already holds.** No inverted
    /// index and no traversal: a superseded draft found the affected sets as a by-product of a pass
    /// that had to visit every (artifact, member) pair, which is the coupling this replaces.
    ///
    /// Two kinds of loss, and they are not the same event:
    ///
    /// - a **generating set** that lost a member describes content generated from a document that
    ///   no longer exists. Under the layer's strict declaration the content and its set are dropped
    ///   at this fold; under permissive the member leaves the set and it serves again. Either way
    ///   the caller is owed the notice, because only they can decide whether the text still says
    ///   something true.
    /// - a **membership** that lost members is smaller than the caller declared it. Nothing is
    ///   wrong with it — every count was already correct at the ack — but a caller planning a
    ///   refresh wants to know which of their sets have drifted.
    ///
    /// **Unmasked counts, deliberately.** This is control-plane output behind the operator
    /// credential and outside the leak register's viewer scope
    /// ([decision 0024](../../../docs/decisions/0024-operator-credential-is-out-of-scope.md)); a
    /// viewer-facing route carrying these numbers would be C8.
    pub fn degradations(&self, retired: &Bitmap) -> Vec<Degradation> {
        let mut out = Vec::new();
        if retired.is_empty() {
            return out;
        }
        for ((layer, level), slots) in &self.levels {
            for (ordinal, slot) in slots.iter().enumerate() {
                let Some(record) = slot else { continue };
                // An artifact this fold retires outright is not *degraded* — it is gone, and its
                // own deletion is what the caller already knows about.
                if retired.contains(record.entity.raw() as u32) {
                    continue;
                }
                let members_lost = record.members.and_cardinality(retired);
                let mut variations_lost = Vec::new();
                for (index, variation) in record.variations.iter().enumerate() {
                    let lost = variation.generated_from.and_cardinality(retired);
                    if lost > 0 {
                        variations_lost.push((index as u32, lost));
                    }
                }
                if members_lost == 0 && variations_lost.is_empty() {
                    continue;
                }
                out.push(Degradation {
                    layer: layer.clone(),
                    level: *level,
                    ordinal: ordinal as u32,
                    stable_key: record.stable_key.clone(),
                    members_lost,
                    declared_members: record.declared_size(),
                    variations_lost,
                });
            }
        }
        out
    }

    /// How many Roaring **containers** every membership holds, summed across every level.
    ///
    /// **The one quantity the fold's artifact pass can be priced from**, and the reason it is
    /// counted rather than modelled: resident cost is ~90 B per container — not per artifact and
    /// not per member — so a membership of a hundred members costs what its *scatter* says and
    /// nothing a manifest holds can predict that
    /// ([the probe](../../../probes/2026-08-16-membership-residency/README.md)). A planner charging
    /// per declared member would overcharge a compact clustering by an order and refuse folds that
    /// fit.
    ///
    /// Counted over the **entity**-space form, which is what this store holds; the pass produces the
    /// row-space form, whose container count differs but tracks it, both being decided by how
    /// scattered a membership is in a Morton-ranked space. That is the approximation in this number,
    /// and it is the honest one available before the pass has run.
    ///
    /// `O(containers)`, on the fold's planning path only — tens of milliseconds at 10⁷ artifacts,
    /// against a fold measured in minutes.
    pub fn membership_containers(&self) -> u64 {
        self.levels
            .values()
            .flat_map(|slots| slots.iter().flatten())
            .map(|record| record.members.statistics().n_containers as u64)
            .sum()
    }

    /// Every level's artifacts **whole**, with `retired` dropped from each membership — what the
    /// fold repacks into the prefix it is publishing.
    ///
    /// **Not [`Self::unpublished`] with a wider range.** A fold publishes a new prefix and extent
    /// paths are prefix-relative, so every artifact has to be written again whether or not it was
    /// already durable somewhere else; the high-water this rewrite leaves behind is the level's own
    /// length. It is also not a copy: a fold retires entities, and a membership carried forward
    /// unchanged goes on counting members that no longer exist — in the very size the proportional
    /// existence criterion divides by.
    ///
    /// **`retired` is the fold's executed deletions and nothing else.** A *suppressed* member stays
    /// in the set: a suppression retires only on unsuppress and never touches a stored structure
    /// (Rule S), so dropping its bit here would give it a second retirement route, which is
    /// fail-open.
    ///
    /// **Generating sets move only where the layer said they may**, which is `policy`
    /// (`annotation-write-cycle.md` §3.2). Under `WithdrawContent` — the default — a variation that
    /// lost a source is **dropped whole**, content and set together, because containment is
    /// all-or-nothing and a set that lost a member fails it for every principal for ever; the caller
    /// regenerates. Under `ShrinkGeneratingSet` the member leaves the set and the content serves
    /// again, which is a channel the caller chose for an object whose membership is statistical.
    /// Neither is a service *behaviour*: both are the declaration executing.
    ///
    /// A level with a hole is reported rather than packed around, exactly as in
    /// [`Self::unpublished`] — but the consequence differs and the caller must not treat it as a
    /// skip: an extent this rewrite omits is a level the new prefix does not carry at all, whose
    /// artifacts come back registered, addressable and served as absent.
    pub fn repack_all(
        &self,
        retired: &Bitmap,
        policy: &dyn Fn(&str) -> OnMemberDeletion,
    ) -> Vec<PendingExtent> {
        let mut ready = Vec::new();
        for ((layer, level), slots) in &self.levels {
            if slots.is_empty() {
                continue;
            }
            let on_deletion = policy(layer);
            let blobs: Vec<Vec<u8>> = slots
                .iter()
                .map(|slot| {
                    // **An empty blob is a hole, and a hole is a real state** — an artifact this
                    // fold retired, or one whose publication is still in flight. It has to be
                    // *written* rather than packed around: an ordinal is identity, so closing a gap
                    // would hand every later artifact in the level the identity of its neighbour,
                    // and every `tessera_id` a caller holds would name the wrong cluster.
                    let Some(record) = slot else {
                        return Vec::new();
                    };
                    // **Rule F's artifact arm.** An artifact whose own entity this fold executed
                    // leaves the level here, in the same publication that retires the overlay entry
                    // hiding it — which is the ordering the rule is about, not reclamation. A
                    // deleted artifact has no rows and no postings, so compaction's derivation
                    // would otherwise call it executed *vacuously* at the first fold and retire the
                    // entry while its slot went on being served.
                    if retired.contains(record.entity.raw() as u32) {
                        return Vec::new();
                    }
                    if retired.is_empty() {
                        return encode_record(record);
                    }
                    let mut record = record.clone();
                    record.members.andnot_inplace(retired);
                    apply_deletion_policy(&mut record, retired, on_deletion);
                    encode_record(&record)
                })
                .collect();
            ready.push((layer.clone(), *level, 0, blobs));
        }
        ready
    }

    /// Drop every artifact this fold executed, and the entities it retired from what survives.
    ///
    /// The resident half of [`Self::repack_all`], applied after the flip for the reason
    /// [`Self::mark_published`] is: until the manifest naming the rewritten extents is durable, the
    /// old prefix is what a restart opens.
    ///
    /// **A retired artifact's slot becomes a hole rather than disappearing**, and its stable key
    /// goes with it — the key indexes an ordinal, and a key left behind would resolve a caller's
    /// republication onto the identity of the artifact this fold just removed.
    pub fn retire(&mut self, retired: &Bitmap, policy: &dyn Fn(&str) -> OnMemberDeletion) {
        if retired.is_empty() {
            return;
        }
        for ((layer, level), slots) in self.levels.iter_mut() {
            let on_deletion = policy(layer);
            for slot in slots.iter_mut() {
                let Some(record) = slot else { continue };
                if retired.contains(record.entity.raw() as u32) {
                    if let Some(key) = &record.stable_key {
                        self.keys.remove(&(layer.clone(), *level, key.clone()));
                    }
                    *slot = None;
                    continue;
                }
                record.members.andnot_inplace(retired);
                apply_deletion_policy(record, retired, on_deletion);
            }
        }
        // Every row-space projection built from these is now wrong in both directions — memberships
        // that shrank, and artifacts that are gone.
        self.version += 1;
    }

    /// The supplied content of every artifact not yet in a manifest, as `(entity, tagged values)`.
    ///
    /// **Tags are `variation × kinds + kind`, positions in the artifact's own layer declaration** —
    /// the same idiom a point row's tags follow, where a tag is a position in the manifest's
    /// declared scalars. Artifact rows and point rows therefore share one store and one reader
    /// while each reads its tags against its own declaration, which is safe because the two never
    /// share an entity: the allocator issues artifact ids downward from the ceiling and point ids
    /// upward from zero, so which declaration governs a row is a range check on its entity.
    ///
    /// Every variation of one artifact carries a value for every declared kind — refused at
    /// publication otherwise — so the stride is the same for all of them and is recoverable from
    /// the layer's declaration alone.
    pub fn unpublished_content(&self) -> Vec<(EntityId, Vec<(u16, String)>)> {
        let mut out = Vec::new();
        for ((layer, level), slots) in &self.levels {
            let from = *self.published_through.get(&(layer.clone(), *level)).unwrap_or(&0) as usize;
            if from >= slots.len() {
                continue;
            }
            for slot in &slots[from..] {
                let Some(record) = slot else { continue };
                let mut fields = Vec::new();
                for (v, variation) in record.variations.iter().enumerate() {
                    let Some(values) = &variation.values else {
                        continue;
                    };
                    for (k, value) in values.iter().enumerate() {
                        let tag = v * values.len() + k;
                        // A layer whose kinds and variations multiply past the tag space cannot be
                        // written back. **The whole artifact's row is abandoned, not the one
                        // field**: the reader requires every declared kind or none, so a row
                        // missing one withholds the artifact — while the *in-memory* copy, tried
                        // first, would go on serving it in full. Dropping the field alone therefore
                        // makes the two copies disagree, and which one a viewer gets depends on
                        // whether the process has restarted since publication.
                        let Ok(tag) = u16::try_from(tag) else {
                            // No logging facade in this crate; the withholding is what a reader
                            // sees, and `supplied_content` refuses on the same condition at the
                            // other end.
                            fields.clear();
                            break;
                        };
                        fields.push((tag, value.clone()));
                    }
                    if fields.is_empty() && !variation.values.as_ref().is_none_or(Vec::is_empty) {
                        // The break above cleared it: abandon this artifact entirely rather than
                        // writing the variations that happened to fit.
                        break;
                    }
                }
                if !fields.is_empty() {
                    out.push((record.entity, fields));
                }
            }
        }
        out
    }

    /// Every level and how many ordinals it currently spans, holes included — what a publication
    /// marks as published once its manifest is durable.
    pub fn levels_and_extents(&self) -> impl Iterator<Item = (&str, u32, u32)> {
        self.levels
            .iter()
            .map(|((layer, level), slots)| (layer.as_str(), *level, slots.len() as u32))
    }

    /// Record that `[0, through)` of a level is durable in a manifest.
    ///
    /// **Called only after the manifest naming the extent is itself durable.** Marking earlier would
    /// let rotation reclaim the log records behind memberships whose file a crash could still lose —
    /// which is the one ordering this whole mechanism exists to get right.
    pub fn mark_published(&mut self, layer: &str, level: u32, through: u32) {
        let entry = self
            .published_through
            .entry((layer.to_string(), level))
            .or_insert(0);
        *entry = (*entry).max(through);
        self.recompute_pin();
    }

    /// The log position of the oldest publication whose memberships are not yet in a manifest.
    ///
    /// **Recomputed from scratch rather than advanced**, because the alternative is an increment
    /// that has to be right at every call site. A level fully published contributes nothing; a level
    /// with anything outstanding contributes the position it was applied at.
    fn recompute_pin(&mut self) {
        let outstanding = self.levels.iter().any(|((layer, level), slots)| {
            let through = *self
                .published_through
                .get(&(layer.clone(), *level))
                .unwrap_or(&0) as usize;
            through < slots.len()
        });
        if !outstanding {
            self.oldest_wal_pos = None;
        }
    }

    /// Seed one artifact from a published extent, **before** WAL replay unions what came after.
    ///
    /// The ordering is the rule and not a preference, exactly as it is for the registry and the
    /// overlay: every WAL record postdates any state a manifest carries, so seeding afterwards would
    /// overwrite a later publication with an earlier one. Seeded artifacts are published by
    /// definition, so this advances the high-water and never the pin.
    /// Declare that a seeded level covers `[0, len)` ordinals, holes included.
    ///
    /// **A hole at the top of a level is invisible from its records alone**, and that is an identity
    /// bug rather than an untidiness: a level seeded only from the blobs that decoded ends *shorter*
    /// than the extent that was written, [`Self::next_ordinal`] regresses onto the hole, and the
    /// next publication is handed the ordinal — and therefore the entity, which is a function of it
    /// — that the artifact this fold deleted was published under. Two artifacts, one `tessera_id`,
    /// with the second answering for the first.
    ///
    /// The extent's own `count` is the authority on how far a level reaches, so it is carried here
    /// rather than inferred. A hole in the *middle* survives without this, which is exactly why it
    /// has to be explicit: the case that hides is the one at the end.
    pub fn seed_extent_bound(&mut self, layer: &str, level: u32, len: u32) {
        let slots = self.levels.entry((layer.to_string(), level)).or_default();
        if slots.len() < len as usize {
            slots.resize_with(len as usize, || None);
        }
        let entry = self
            .published_through
            .entry((layer.to_string(), level))
            .or_insert(0);
        *entry = (*entry).max(len);
    }

    pub fn seed(&mut self, layer: &str, level: u32, ordinal: u32, record: ArtifactRecord) {
        self.put(layer, level, ordinal, record);
        let entry = self
            .published_through
            .entry((layer.to_string(), level))
            .or_insert(0);
        *entry = (*entry).max(ordinal + 1);
    }

    /// How many artifacts are held, across every layer. **Operator-facing only**: a per-layer count
    /// is a corpus-wide count over objects a principal may not individually see, which is C8's row,
    /// and this deliberately offers no way to ask for one.
    pub fn total(&self) -> usize {
        self.levels
            .values()
            .map(|slots| slots.iter().filter(|s| s.is_some()).count())
            .sum()
    }

    pub fn is_empty(&self) -> bool {
        self.total() == 0
    }
}

/// Encode one artifact for a packed extent: its caller key, its membership and its variations'
/// generating sets, in one blob.
///
/// ```text
/// blob       := u16 LE key_len | key bytes (UTF-8)
///             | u16 LE variation_count
///             | u32 LE members_len | membership bytes (portable Roaring)
///             | variation*
///             | attachment
/// variation  := u32 LE set_len | generating-set bytes (portable Roaring)
/// attachment := u8 0                                     -- unattached
///             | u8 1 | u16 LE layer_len | layer bytes (UTF-8)
///                    | u32 LE level | u32 LE ordinal | u64 LE target entity
/// ```
///
/// **The attachment is stored and not re-derived**, on the reason [`Attachment`] gives: it is a
/// term of the visibility predicate, so an artifact restored without it is one that serves where
/// the live copy would withhold — the fail-open the term exists to close, reappearing at a restart.
///
/// **Every length is explicit, including the membership's.** The membership used to be the blob's
/// tail and its length was *"whatever is left"*, which is exactly the shape that decodes a truncated
/// blob as a shorter membership — an artifact with a low masked count for every viewer, which the
/// criterion renders as absent with nothing to notice. With a length in front, short is short.
///
/// **The key travels with the membership because nothing else durable carries it.** An artifact's
/// entity is derivable from its layer's reserved runs and its ordinal, so the extent need not carry
/// it; a caller's stable key is derivable from nothing. Putting it in the manifest instead would put
/// one JSON string per artifact in a document parsed at every open — the entry-count problem the
/// packing exists to solve, in another guise.
///
/// `tessera-store` holds this as an opaque blob and addresses it by ordinal. **That split is the
/// layering**: the store owns which bytes belong to which artifact, this crate owns what the bytes
/// mean, and the bitmap library stays on one side of the boundary.
pub fn encode_record(record: &ArtifactRecord) -> Vec<u8> {
    let key = record.stable_key.as_deref().unwrap_or_default().as_bytes();
    let members = serialise_members(&record.members);
    let sets: Vec<Vec<u8>> = record
        .variations
        .iter()
        .map(|v| serialise_members(&v.generated_from))
        .collect();
    let mut out =
        Vec::with_capacity(8 + key.len() + members.len() + sets.iter().map(Vec::len).sum::<usize>());
    // A key longer than a `u16` cannot round-trip, and truncating one would silently rename an
    // artifact. The control plane bounds the request body long before this, so the clamp is a
    // backstop; it refuses at encode rather than writing a key it cannot read back.
    let key_len = u16::try_from(key.len()).unwrap_or(u16::MAX);
    out.extend_from_slice(&key_len.to_le_bytes());
    if key_len != u16::MAX {
        out.extend_from_slice(key);
    }
    // Same argument, one level up: more variations than a `u16` can count is a publication this
    // encoding cannot read back, so it refuses rather than writing a prefix of the ranking. A
    // dropped variation is a viewer served a *different* description from the one the caller
    // ranked for them.
    let count = u16::try_from(sets.len()).unwrap_or(u16::MAX);
    out.extend_from_slice(&count.to_le_bytes());
    out.extend_from_slice(&(members.len() as u32).to_le_bytes());
    out.extend_from_slice(&members);
    if count != u16::MAX {
        for set in &sets {
            out.extend_from_slice(&(set.len() as u32).to_le_bytes());
            out.extend_from_slice(set);
        }
    }
    match &record.attached_to {
        None => out.push(0),
        Some(attachment) => {
            let layer = attachment.layer.as_bytes();
            // Same argument as the key's, and stricter in consequence: a layer name that could not
            // round-trip would restore an attached artifact as an unattached one, which serves it
            // where the live copy withholds it. Refused at encode, which the decoder then reports.
            let Ok(layer_len) = u16::try_from(layer.len()) else {
                out.push(u8::MAX);
                return out;
            };
            out.push(1);
            out.extend_from_slice(&layer_len.to_le_bytes());
            out.extend_from_slice(layer);
            out.extend_from_slice(&attachment.level.to_le_bytes());
            out.extend_from_slice(&attachment.ordinal.to_le_bytes());
            out.extend_from_slice(&attachment.entity.raw().to_le_bytes());
        }
    }
    // The parent, on the attachment's discriminant rule: absent is one byte and never zero, so
    // *this node is a root* and *this reader does not know whether it had a parent* cannot encode
    // the same. Here the second answer would serve a child beside the ancestor that should have
    // replaced it — a duplicate on the map rather than a disclosure, but wrong either way.
    match record.parent {
        None => out.push(0),
        Some(parent) => {
            out.push(1);
            out.extend_from_slice(&parent.level.to_le_bytes());
            out.extend_from_slice(&parent.ordinal.to_le_bytes());
        }
    }
    out
}

/// The inverse, refusing anything it cannot read back exactly.
///
/// **A refusal and never a partial record**, on [`deserialise_members`]'s argument: an artifact whose
/// key was lost is one no edge can name, and an artifact whose membership decoded short is one with
/// a low masked count for every viewer — which the existence criterion renders as *absent*, with no
/// error anywhere to notice. Both must be a decode failure the caller alarms on.
pub fn decode_record(entity: EntityId, blob: &[u8]) -> Option<ArtifactRecord> {
    let mut at = 0usize;
    let mut take = |n: usize| -> Option<&[u8]> {
        let end = at.checked_add(n)?;
        let view = blob.get(at..end)?;
        at = end;
        Some(view)
    };
    let key_len = u16::from_le_bytes(take(2)?.try_into().ok()?) as usize;
    if key_len == u16::MAX as usize {
        return None;
    }
    let stable_key = if key_len == 0 {
        None
    } else {
        Some(std::str::from_utf8(take(key_len)?).ok()?.to_string())
    };
    let count = u16::from_le_bytes(take(2)?.try_into().ok()?) as usize;
    if count == u16::MAX as usize {
        return None;
    }
    let members_len = u32::from_le_bytes(take(4)?.try_into().ok()?) as usize;
    let members = deserialise_members(take(members_len)?)?;
    let mut variations = Vec::with_capacity(count);
    for _ in 0..count {
        let set_len = u32::from_le_bytes(take(4)?.try_into().ok()?) as usize;
        variations.push(VariationSet {
            // ⊘ The extent carries no values — see [`VariationSet::values`]. A restored variation is
            // therefore unservable until the blob write lands, which is fail-closed and loud rather
            // than an artifact served with its description missing.
            values: None,
            generated_from: deserialise_members(take(set_len)?)?,
        });
    }
    // An attachment absent is one byte and never zero bytes: *unattached* and *this reader does not
    // know whether it was attached* must not encode the same, since the second answer is one that
    // serves a label whose cluster is hidden.
    let attached_to = match take(1)?[0] {
        0 => None,
        1 => {
            let layer_len = u16::from_le_bytes(take(2)?.try_into().ok()?) as usize;
            let layer = std::str::from_utf8(take(layer_len)?).ok()?.to_string();
            let level = u32::from_le_bytes(take(4)?.try_into().ok()?);
            let ordinal = u32::from_le_bytes(take(4)?.try_into().ok()?);
            let entity = EntityId::new(u64::from_le_bytes(take(8)?.try_into().ok()?));
            Some(Attachment {
                layer,
                level,
                ordinal,
                entity,
            })
        }
        _ => return None,
    };
    let parent = match take(1)?[0] {
        0 => None,
        1 => Some(crate::wal::ParentRef {
            level: u32::from_le_bytes(take(4)?.try_into().ok()?),
            ordinal: u32::from_le_bytes(take(4)?.try_into().ok()?),
        }),
        _ => return None,
    };
    // **Trailing bytes are a decode failure**, not slack to ignore: a blob longer than its own
    // structure means the writer and this reader disagree about the format, and the half that
    // decoded cleanly is the more dangerous outcome of the two.
    if at != blob.len() {
        return None;
    }
    Some(ArtifactRecord {
        entity,
        stable_key,
        members,
        variations,
        attached_to,
        parent,
    })
}

/// Serialise a membership for the WAL, in CRoaring's portable form.
///
/// Portable rather than the frozen form the fragment cache uses: frozen is an mmap-oriented layout
/// with alignment padding and no cross-version guarantee, and this goes into a log that must be
/// readable by the process that reopens it. The bytes are self-describing enough that a corrupt
/// record fails to deserialise rather than yielding a plausible wrong set.
pub fn serialise_members(members: &Bitmap) -> Vec<u8> {
    members.serialize::<Portable>()
}

/// The inverse, refusing bytes that are not a bitmap.
///
/// **A refusal, not a default.** An empty membership is a real and meaningful state — an artifact
/// every one of whose members has been deleted — so decoding damage to "empty" would make a
/// corrupted record indistinguishable from a legitimately emptied artifact, and the second is
/// served rather than refused.
pub fn deserialise_members(bytes: &[u8]) -> Option<Bitmap> {
    Bitmap::try_deserialize::<Portable>(bytes)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn record(entity: u64, members: &[u32]) -> ArtifactRecord {
        ArtifactRecord {
            entity: EntityId::new(entity),
            stable_key: None,
            members: Bitmap::of(members),
            variations: Vec::new(),
            attached_to: None,
            parent: None,
        }
    }

    #[test]
    fn a_level_is_dense_and_a_hole_answers_absent() {
        // A publication that skips an ordinal — one artifact still in flight, or one a fold has
        // removed — must leave a hole that answers `None`, not a panic and not a neighbour.
        let mut store = ArtifactStore::new();
        store.put("clusters/a", 0, 5, record(100, &[1, 2, 3]));
        assert!(store.get("clusters/a", 0, 5).is_some());
        assert!(store.get("clusters/a", 0, 0).is_none());
        assert!(store.get("clusters/a", 0, 9).is_none());
        assert_eq!(store.level("clusters/a", 0).count(), 1);
        assert_eq!(store.total(), 1);
    }

    #[test]
    fn the_declared_size_is_derived_and_cannot_drift() {
        // It is the proportional criterion's denominator and nothing else. Deriving it means a
        // deletion that shrinks the membership shrinks the denominator with it, in one place.
        let mut r = record(100, &[1, 2, 3, 4]);
        assert_eq!(r.declared_size(), 4);
        r.members.remove(3);
        assert_eq!(r.declared_size(), 3);
    }

    #[test]
    fn dropping_a_layer_takes_its_artifacts_with_it() {
        let mut store = ArtifactStore::new();
        store.put("clusters/a", 0, 0, record(100, &[1]));
        store.put("clusters/a", 1, 0, record(101, &[2]));
        store.put("clusters/b", 0, 0, record(102, &[3]));
        assert_eq!(store.total(), 3);

        store.remove_layer("clusters/a");
        assert_eq!(store.total(), 1);
        assert!(store.get("clusters/b", 0, 0).is_some());
        assert_eq!(store.layer("clusters/a").count(), 0);
    }

    #[test]
    fn a_membership_round_trips_and_damage_is_refused_rather_than_emptied() {
        let members = Bitmap::of(&[1, 2, 3, 70_000, 4_000_000]);
        let bytes = serialise_members(&members);
        assert_eq!(deserialise_members(&bytes), Some(members));

        // An empty membership is a real state — every member deleted — so damage must not decode
        // to it. That would make a corrupt record indistinguishable from a legitimately emptied
        // artifact, and the second is served.
        let empty = Bitmap::new();
        assert_eq!(deserialise_members(&serialise_members(&empty)), Some(empty));
        assert_eq!(deserialise_members(&[0xff, 0xff, 0xff, 0xff]), None);
        assert_eq!(deserialise_members(&[]), None);
    }

    /// **An attachment survives the packed extent, and a lost one is a decode failure.** A label
    /// restored as unattached is a label that serves when its cluster is suppressed — the fail-open
    /// the term exists to close, reappearing at a restart, with nothing anywhere reporting a fault.
    #[test]
    fn an_attachment_round_trips_and_a_truncated_one_is_refused() {
        let mut r = record(100, &[1, 2, 3]);
        r.stable_key = Some("l0".into());
        r.variations = vec![VariationSet {
            values: Some(vec!["a label".into()]),
            generated_from: Bitmap::of(&[1, 2]),
        }];
        r.attached_to = Some(Attachment {
            layer: "clusters/a".into(),
            level: 0,
            ordinal: 17,
            entity: EntityId::new(4_294_901_759),
        });

        let blob = encode_record(&r);
        let back = decode_record(r.entity, &blob).expect("a whole blob decodes");
        assert_eq!(back.attached_to, r.attached_to);
        assert_eq!(back.stable_key, r.stable_key);

        // An unattached artifact round-trips too, carrying its one absence byte — *unattached* and
        // *this reader could not tell* must not encode the same.
        let mut plain = r.clone();
        plain.attached_to = None;
        let plain_blob = encode_record(&plain);
        let restored = decode_record(plain.entity, &plain_blob).unwrap();
        assert_eq!(restored.attached_to, None);
        assert_eq!(restored.members, plain.members);

        // Every truncation from the end of the variations onwards refuses. The one that matters is
        // the shortest: it is byte-for-byte the unattached artifact's blob without its absence
        // byte, and a reader that shrugged at a missing tail would decode it as unattached.
        for len in (plain_blob.len() - 1)..blob.len() {
            assert!(
                decode_record(r.entity, &blob[..len]).is_none(),
                "a blob truncated to {len} of {} bytes must refuse",
                blob.len()
            );
        }
    }

    #[test]
    fn layers_do_not_bleed_into_each_other_in_key_order() {
        // The `layer` iterator walks a range of a BTreeMap keyed by `(name, level)`, so a
        // neighbouring name that sorts adjacently must not be picked up.
        let mut store = ArtifactStore::new();
        store.put("clusters/a", 0, 0, record(100, &[1]));
        store.put("clusters/a-suffix", 0, 0, record(101, &[2]));
        store.put("clusters/b", 0, 0, record(102, &[3]));
        assert_eq!(store.layer("clusters/a").count(), 1);
        assert_eq!(store.layer("clusters/a-suffix").count(), 1);
    }
}
