//! What a caller declares when they register an annotation layer.
//!
//! A **layer** is what shares a gate and a lifecycle (`annotations.md` §2.1). This module is the
//! declaration's only definition: the WAL record that makes a registration durable, the manifest
//! section that carries it across a build, and the gate-filtered `/v1/meta` view all read this one
//! type, so a field cannot mean one thing on disk and another on the wire.
//!
//! ## Two axes, and only two
//!
//! Every question about who may see a layer or an artifact in it answers one of two
//! ([decision 0088](../../../docs/decisions/0088-visibility-is-two-axes-and-the-membership-test-is-one.md)):
//! [`LayerDeclaration::visibility`] asks which access label the viewer must hold, and
//! [`LayerDeclaration::require_member_visibility`] asks how much of the object's own membership the
//! viewer must already see. The second **requires** members to be visible and never *sets* their
//! visibility — a container grants its members nothing, and the reverse reading inverts the
//! direction the system exists to protect.
//!
//! [`ArtifactVisibility::field`] and [`SuppliedContent::require_member_visibility`] are the two the
//! leak register watches (C27, C28). Both are **explicit and required**: neither has a default to
//! fall through, because the failure in each case is silent. A corpus-derived clustering declared as
//! carrying its own labels serves the existence and count of every cluster down to one member;
//! corpus-derived content declared corpus-independent is served with no containment test at all,
//! which is the disclosure the containment rule exists to prevent.
//!
//! **The absence of a rule is itself a declaration.** [`LayerDeclaration::require_member_visibility`]
//! being `None` — the word `none` in the config — says *this layer needs no membership requirement*,
//! a claim a reviewer can check, rather than *nobody filled this in*. That is why it has no default
//! and why a criterion is never inherited from a deployment-wide setting: a control that can be
//! arrived at by accident from an unrelated choice is the shape that shipped a fail-open once
//! already, in the three gate modes this replaced (decision 0079).
//!
//! ## No `skip_serializing_if` on anything here, ever
//!
//! This type is written to the WAL as **postcard**, which is not self-describing: fields are
//! decoded by position, with no names on the wire. A `skip_serializing_if` that omits an absent
//! `Option` therefore shortens the record, and every field after it decodes from the wrong bytes —
//! a layer replaying with a different gate, or a `WalCorruption` if the shift happens to be
//! unparseable. The second is the lucky outcome.
//!
//! It is an easy change to make for JSON tidiness and it costs `"label": null` to avoid, so the
//! rule is absolute rather than case-by-case. `#[serde(default)]` is fine and different: it affects
//! only formats that can tell a field is missing, and postcard never omits one.
//!
//! ## Lineage and levels are independent
//!
//! [`Hierarchy::kind`] says where a layer's lineage lives; [`LayerDeclaration::levels`] says what
//! resolutions it declares. **Neither carries the other** (decision 0082). A clustering is a tree in
//! its edges and declares no levels, because a condensed tree is unbalanced and a level number would
//! say nothing about position in the lineage. A tiered geography declares both, and they
//! agree, because a ward is a ward everywhere on the map — which is the only shape in which reading
//! one as the other is safe.

use std::collections::BTreeSet;

use serde::{Deserialize, Serialize};

/// Where a layer's artifacts get their membership. Levels inherit it — a layer is enumerated or
/// predicate-backed as a whole, never per level, because the membership source decides what a write
/// invalidates and a layer is the unit of lifecycle.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum MembershipSource {
    /// A stored set of entities per artifact. Stale between the write and the refresh that rebuilds
    /// it, which is fail-closed: an unrebuilt member has no bit, so every masked count understates.
    Enumerated,
    /// A shape, decomposed to Morton ranges at request time. Never stale — a point ingested inside
    /// a boundary is a member on the next request with nothing rebuilt.
    Spatial,
    /// A predicate over an existing value column. Never stale, for the same reason.
    Attribute,
}

/// Where each artifact's own access label is, and what one carrying none gets.
/// **The register watches this** (C27).
///
/// **The presence of [`ArtifactVisibility::field`] is the declaration that artifacts carry their
/// own labels** — what the retired `artifacts_carry_own` said, spelled as the thing that makes it
/// true rather than as a second flag beside it (decision 0088). A layer that names no field has
/// artifacts whose existence is derived from their members' visibility, which is the
/// membership-derivation rule points already obey.
///
/// **No default on either half.** Mis-declared as carrying its own labels, a corpus-derived layer
/// serves the existence of every artifact to every principal who reaches it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ArtifactVisibility {
    /// The field each artifact's own access label is read from. `None` — no field, so no artifact
    /// carries a label of its own.
    ///
    /// ⊘ **Acquisition, and nothing reads it yet**: the build has no artifact-label column and the
    /// control plane takes no label per artifact, so today only its *presence* is consulted
    /// ([`ArtifactVisibility::carry_own`]). Naming a field therefore declares the shape without yet
    /// filling it — which is fail-closed, an artifact with no label being withheld.
    pub field: Option<String>,
    /// What an artifact carrying no label of its own gets.
    pub default: MemberDefault,
}

/// What a member carrying no label of its own gets — the fallback half of an
/// [`ArtifactVisibility`] or of a view's point visibility.
///
/// **Filling never overrides**: a member carrying its own label keeps exactly that, and this lands
/// only where the field is null or empty.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum MemberDefault {
    /// The container's own gate is the whole of it. Legal for artifacts and **not** for points: a
    /// point carrying no terms is in no posting list and so in no principal's mask, and a gate
    /// narrows rather than widens.
    Inherited,
    /// An access label, `public` included — `public` is a label rather than a reserved absence
    /// (`per-point-attributes.md` §3.8).
    Label(String),
}

impl ArtifactVisibility {
    /// Whether artifacts on this layer carry access labels of their own — the field's presence,
    /// which is the whole of what C27 watches.
    pub fn carry_own(&self) -> bool {
        self.field.is_some()
    }

    /// Artifacts carry no labels; the layer's own gate is the whole of it.
    pub fn inherited() -> Self {
        ArtifactVisibility {
            field: None,
            default: MemberDefault::Inherited,
        }
    }

    /// Artifacts carry their own labels in `field`, and one carrying none inherits the layer's
    /// gate.
    pub fn carried(field: impl Into<String>) -> Self {
        ArtifactVisibility {
            field: Some(field.into()),
            default: MemberDefault::Inherited,
        }
    }
}

/// The masked count an artifact must clear to be **served at all**.
///
/// It never modifies a number: the count beside a served artifact is the masked count, unmodified.
/// What it decides is whether the artifact exists for this viewer (decision 0075), and it is
/// independent of [`ArtifactVisibility`] — a layer may declare both, either or neither.
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ExistenceCriterion {
    /// Serve iff the masked count is at least this many visible members. The config spells it
    /// `require_member_visibility = { count = n }`, and `"any"` is this at `n = 1`.
    ///
    /// **The form under which rollup is guaranteed.** A child's members are a subset of its
    /// parent's, so its masked count is never larger: a child that fails while its parent passes
    /// leaves the parent served, and nobody is left with a blank region.
    Count(u64),
    /// Serve iff the masked count is at least this fraction of the artifact's **declared**
    /// membership size. `0.0 < p <= 1.0`. The config spells it
    /// `require_member_visibility = { fraction = p }`, and `"all"` is this at `p = 1.0`.
    ///
    /// **The form that scales** — a fixed bar of fifty protects a cluster of a hundred and does
    /// nothing for a cluster of ten thousand — and the form that ⊘ **breaks rollup**: a ratio does
    /// not shrink downward, so a parent at 5% of 10 000 declared members fails a 10% rule while its
    /// child at 50% of 200 passes it, the child a strict subset throughout. A layer declaring this
    /// must expect gaps in its lineage. No disclosure follows — each artifact passed its own test.
    ///
    /// ⊘ **It has no denominator for predicate membership** and is refused on such a layer until
    /// the owner rules: *"the points inside this shape"* declares no member set, and its size
    /// changes at every write.
    Fraction(f64),
}

/// Where a layer's lineage lives.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum HierarchyKind {
    /// No lineage and no sub-structure — one population of artifacts.
    Flat,
    /// A tree, held in the layer's **edges**. A coarser view is an ancestor.
    Nested,
    /// Independent analyses, one per level, with no lineage between them. A coarser view is a
    /// different analysis rather than an ancestor, so switching to it replaces one claim with
    /// another rather than coarsening the first.
    Stacked,
    /// Levels **and** containment edges between them: each tier sits inside the one above it. A
    /// ward is a ward everywhere on the map, so the resolution is semantic and balanced, and an
    /// edge always runs from a coarser level to a finer one.
    ///
    /// **Named for the structure rather than for a domain.** Administrative boundaries are the
    /// motivating case and the map industry's own word for their levels is *admin level* — but a
    /// subject taxonomy and a biological classification are the same shape, and the first layer
    /// published against this one is arXiv's category tree. `stacked` and `tiered` are the two
    /// levelled shapes, and the difference is audible: piled up independently, against ordered
    /// strata that relate.
    ///
    /// **Its edges are information, not roll-up** *(owner ruling, 2026-08-18)*, and that is the
    /// whole difference from [`Nested`](HierarchyKind::Nested). A treed layer's edges are what a
    /// budget climbs: substituting a parent cluster for its children is an honest coarsening,
    /// because a cluster is an abstract blob. Substituting a state for its counties is not — it
    /// draws one large polygon across a region whose neighbours are still drawn as counties, an
    /// inconsistent map from a server trying to be helpful. So the cut never climbs these edges,
    /// **resolution is the client choosing a level**, and what the edges are for is telling a
    /// client what contains what: nesting the features it draws, or filtering to one subtree while
    /// still drawing the rest.
    ///
    /// A budget is therefore **inert** on such a layer, exactly as it is on a flat one — there is
    /// no depth to trade. An over-large response is the artifact ceiling's business, which refuses
    /// rather than truncating; the cut must never start sampling to reach a number.
    Tiered,
}

/// How a layer's artifacts relate to each other, and what a response does when several pass.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct Hierarchy {
    pub kind: HierarchyKind,
    /// The layer's **default** cut depth policy, not its only setting: where a parent and a child
    /// both pass, serve only the deepest passing artifact per branch. A request may ask for more
    /// detail than the default (decision 0083).
    ///
    /// **This carries no disclosure argument in either direction**, which is unusual enough here to
    /// be worth stating: every artifact served has passed its own test independently, so serving
    /// the frontier serves strictly *less*, and serving every passer reveals nothing beyond what
    /// each artifact's own presence already does. It is decided on rendering grounds.
    #[serde(default)]
    pub prune_children: bool,
}

/// One kind of content a caller supplies on this layer's artifacts.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SuppliedContent {
    /// Distinguishes two contents of one type on one layer — a curated boundary and a statistical
    /// label may both be `polygon`.
    pub name: String,
    /// What it is — `text`, `polygon`, `extent`, `point`. Published in `/v1/meta` so a client knows
    /// what to draw; publishing the *types* is safe because an artifact failing containment is
    /// absent whole, so no served artifact ever lacks a content its layer declares.
    #[serde(rename = "type")]
    pub ty: String,
    /// How much of the generating set a viewer must already see. **The register watches this
    /// field** (C28), and it has no default.
    pub require_member_visibility: SuppliedRequirement,
}

/// The membership requirement one supplied content carries — the second axis, at the only two
/// settings supplied content admits (`configuration.md` §1).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SuppliedRequirement {
    /// The content asserts something about documents, so it is served only to a viewer who can see
    /// **everything** it was generated from. Such content **must** arrive with a generating set;
    /// one that does not is refused.
    All,
    /// The content is true whether or not a single document exists, so containment is vacuous and
    /// it serves on the container's own gate alone. Such content must **not** declare a generating
    /// set: a set that is never tested is a claim the service would carry without meaning (C28).
    Inherited,
}

impl SuppliedRequirement {
    /// Whether this content was generated from corpus items, and so must clear containment.
    pub fn is_corpus_derived(self) -> bool {
        matches!(self, SuppliedRequirement::All)
    }
}

/// One member of the closed vocabulary of properties the engine recomputes per viewer.
///
/// **The vocabulary lives here, beside the declaration that names it**, so the set a caller may
/// declare and the set the engine implements are one list rather than two that can drift — a
/// declared property nothing computes would be silently absent from every artifact.
///
/// The masked **count** is not here: it is intrinsic, every artifact has one, and the existence
/// criterion requires it computed regardless. Everything below is opt-in because it costs
/// O(visible members) per artifact per request against the count's O(containers touched).
///
/// ⊘ **`extractive_terms` is specified and not implemented** (`annotations.md` §4.2, marked per
/// [decision 0013](../../../docs/decisions/0013-mark-specified-vs-implemented.md)). It is not in
/// this list, so a layer declaring it is **refused at registration** rather than registered and
/// served without it.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ComputedProperty {
    /// The mean position of the visible members.
    Centroid,
    /// The axis-aligned bounds of the visible members.
    Box,
    /// The convex hull of the visible members.
    Hull,
}

impl ComputedProperty {
    pub fn parse(name: &str) -> Option<Self> {
        match name {
            "centroid" => Some(ComputedProperty::Centroid),
            "box" => Some(ComputedProperty::Box),
            "hull" => Some(ComputedProperty::Hull),
            _ => None,
        }
    }

    pub fn name(self) -> &'static str {
        match self {
            ComputedProperty::Centroid => "centroid",
            ComputedProperty::Box => "box",
            ComputedProperty::Hull => "hull",
        }
    }

    /// Every name a declaration may carry.
    pub const VOCABULARY: [&'static str; 3] = ["centroid", "box", "hull"];
}

/// What a layer's artifacts carry.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ContentDeclaration {
    /// Properties recomputed per viewer from `membership ∩ M_auth` and nothing else — `centroid`,
    /// `hull`, `box`, `extractive_terms`. Contained by construction, so they take no visibility
    /// declaration and pass containment automatically. The masked count is intrinsic and is never
    /// declared here.
    #[serde(default)]
    pub computed: Vec<String>,
    #[serde(default)]
    pub supplied: Vec<SuppliedContent>,
    /// Drop supplied content when one of its generating set is deleted, rather than shrinking the
    /// set. **Defaulted `true`**, and that is the one direction a disclosure control may default
    /// in: the widening half is the one that must be typed (C7).
    ///
    /// `true` — the content and its generating set are dropped together at the fold and the caller
    /// regenerates. Right where the exact membership *is* the object: a curated set, a case file.
    /// Containment being all-or-nothing, a set that loses a member would otherwise fail for every
    /// principal for ever.
    ///
    /// `false` — the fold removes the deleted member and the content goes on serving. Right where
    /// the membership is statistical, and it means a principal satisfying the survivors may read
    /// content generated from the deleted item.
    #[serde(default = "yes")]
    pub withdraw_on_member_deletion: bool,
}

/// [`ContentDeclaration::withdraw_on_member_deletion`]'s default, which is **not** `bool::default`.
/// Deriving `Default` on the struct would give it `false` — the widening half — so the derive is
/// replaced by the impl below rather than left to be silently wrong.
fn yes() -> bool {
    true
}

impl Default for ContentDeclaration {
    fn default() -> Self {
        ContentDeclaration {
            computed: Vec::new(),
            supplied: Vec::new(),
            withdraw_on_member_deletion: true,
        }
    }
}

/// One declared resolution. Present only on layers whose resolutions are semantic and balanced —
/// a tiered geography — or whose levels are independent analyses. **A treed layer declares
/// none** and sits entirely at level 0 (decision 0082).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct LevelDeclaration {
    pub level: u32,
    /// Human-readable, served as metadata. **Optional, like every other title in the surface**: a
    /// title discloses nothing a name does not, and the name is already served, so requiring one
    /// buys nothing. Absent is served as absent rather than as the name — choosing to display an
    /// identity like `clusters/hdbscan` is a client's call, not something the service manufactures.
    pub title: Option<String>,
    /// Advisory min/max zoom, as every tile schema carries. **It bounds no work** — what bounds a
    /// treed layer's response is the request's artifact budget, and what bounds a levelled layer's
    /// is the level asked for.
    pub zoom: Option<(u32, u32)>,
}

/// A caller's complete layer registration.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct LayerDeclaration {
    /// The layer's identity. Tombstoned on drop and **never reused** — a name that once meant
    /// something must not come to mean something else, since bookmarks, edges and suppressions all
    /// travel by it.
    pub name: String,
    /// Human-readable, served as metadata. Optional — see [`LevelDeclaration::title`].
    pub title: Option<String>,
    /// Which views this layer appears in.
    pub views: Vec<String>,
    pub membership: MembershipSource,
    /// The access label a viewer must hold to know this layer exists at all, independent of any
    /// member. `None` is the config's `visibility = "public"` — reachable by every principal.
    ///
    /// Reachability is resolved once per session and keyed on the layer version, with a live
    /// suppression check on the layer's own entity ahead of the cached resolution. A gate-failed
    /// name and a never-registered name are indistinguishable in outcome **and in work**.
    ///
    /// ⊘ `public` is spelled as an absence here and is specified as a **term**, reserved at `0` and
    /// satisfied inside the trust boundary (decision 0088). Until the dictionary carries it, the
    /// absence is what makes the layer reachable — same outcome, and nothing evaluates a term for
    /// it yet.
    pub visibility: Option<String>,
    pub artifact_visibility: ArtifactVisibility,
    /// How much of an artifact's own membership a viewer must already see for it to exist for them.
    /// `None` declares *no such rule*, which is a statement rather than an omission.
    pub require_member_visibility: Option<ExistenceCriterion>,
    pub hierarchy: Hierarchy,
    #[serde(default)]
    pub content: ContentDeclaration,
    // ⊘ **`withdraw_on_member_deletion` on the *layer* is not a field here**, and its absence is
    // the point (`annotation-write-cycle.md` §6.1, decision 0013). It would drop the whole
    // artifact when one member is deleted, and the fold has no such path — so the declaration
    // surface refuses `true` at parse rather than carrying a field the fold would silently ignore.
    // The **content**-level key of the same name is real and lives on `ContentDeclaration`.
    /// The layers this one's edges point into. A layer named here needs stable keys, because an
    /// edge names its target and at publish time the caller has no `tessera_id` for it.
    #[serde(default)]
    pub depends_on: Vec<String>,
    /// Empty for a treed or flat layer.
    #[serde(default)]
    pub levels: Vec<LevelDeclaration>,
}

/// The width a reserved run is aligned and sized to: one Roaring container.
///
/// **Alignment is what keeps a level's bitmap arithmetic cheap.** The measured cost model is that
/// bitmap operations cost O(containers touched) rather than O(cardinality), so a level whose
/// entities sit inside whole containers pays for the containers it fills; one straddling a boundary
/// pays for a partial container at each end, on every operation, for ever.
pub const RESERVED_BLOCK: u64 = 1 << 16;

/// Where the row-less region starts counting **down** from: the highest [`RESERVED_BLOCK`] boundary
/// at or below the entity-id ceiling (`u32::MAX`, contracts §2.6).
///
/// **The 65 535 ids above it are deliberately unusable.** Row-less allocation hands out whole
/// aligned blocks so that a level's membership sits inside whole Roaring containers; starting at
/// `u32::MAX` would make the *first* block the one that straddles a boundary, which is the case
/// alignment exists to remove. One block out of 65 536 is the price.
///
/// It lives here rather than with the allocator because it is a fact about how entity space is
/// divided, which several crates need and only one of them allocates.
pub const ROWLESS_CEILING: u64 = (u32::MAX as u64) & !(RESERVED_BLOCK - 1);

/// A contiguous, ascending run of reserved row-less entity ids.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct EntityRun {
    /// Inclusive, and a multiple of [`RESERVED_BLOCK`].
    pub start: u64,
    /// Exclusive.
    pub end: u64,
}

impl EntityRun {
    pub fn len(&self) -> u64 {
        self.end.saturating_sub(self.start)
    }

    pub fn is_empty(&self) -> bool {
        self.end <= self.start
    }

    pub fn contains(&self, entity: u64) -> bool {
        entity >= self.start && entity < self.end
    }
}

/// The entity ids one level holds, as a **list** of runs in allocation order.
///
/// **A list rather than one block, and that is not an optimisation.** A level that fills its
/// reservation is extended by appending another block, and by then other layers have taken the ids
/// immediately below it — the allocator is monotone downward and hands nobody a reserved gap. So a
/// level's second block is *somewhere* below its first, not adjacent to it, and any scheme assuming
/// one contiguous run either refuses to grow or reissues ids. Both are worse than a short walk.
///
/// **The representation's `ordinal = entity − entity_base` is this with one run**, which is the
/// common case: a level fits its first block until it holds more than 65 536 artifacts. The general
/// form keeps the property that mattered — no per-artifact lookup table in either direction — and
/// pays a walk over a handful of runs instead.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct ReservedRuns {
    runs: Vec<EntityRun>,
}

impl ReservedRuns {
    /// Builds from runs **in allocation order**, which is the order ordinals follow. Ascending
    /// entity order would be wrong: the second block allocated sits at a *lower* address than the
    /// first, so sorting would silently renumber every artifact in the level.
    pub fn from_runs(runs: Vec<EntityRun>) -> Self {
        ReservedRuns { runs }
    }

    pub fn runs(&self) -> &[EntityRun] {
        &self.runs
    }

    pub fn is_empty(&self) -> bool {
        self.capacity() == 0
    }

    /// How many artifacts this level can address.
    pub fn capacity(&self) -> u64 {
        self.runs.iter().map(EntityRun::len).sum()
    }

    /// The entity backing `ordinal`, or `None` if the level has not reserved that far.
    pub fn entity_of(&self, ordinal: u64) -> Option<u64> {
        let mut remaining = ordinal;
        for run in &self.runs {
            if remaining < run.len() {
                return Some(run.start + remaining);
            }
            remaining -= run.len();
        }
        None
    }

    /// The ordinal `entity` addresses, or `None` if this level does not hold it.
    ///
    /// The inverse of [`ReservedRuns::entity_of`] and tested as such: an addressing scheme whose
    /// two directions disagree is one that serves the wrong artifact rather than failing.
    pub fn ordinal_of(&self, entity: u64) -> Option<u64> {
        let mut base = 0u64;
        for run in &self.runs {
            if run.contains(entity) {
                return Some(base + (entity - run.start));
            }
            base += run.len();
        }
        None
    }

    /// Appends a block. Callers pass what the allocator returned, so alignment is the allocator's
    /// property; this asserts it rather than enforcing it, because a misaligned run here means the
    /// allocator is wrong and a silent fixup would hide that.
    pub fn push(&mut self, run: EntityRun) {
        debug_assert_eq!(run.start % RESERVED_BLOCK, 0, "reserved runs are block-aligned");
        debug_assert_eq!(run.len() % RESERVED_BLOCK, 0, "reserved runs are whole blocks");
        self.runs.push(run);
    }
}

/// A registered layer, as it is held in memory and written to a manifest.
///
/// **The ids are stored, never re-derived.** A registration must land on the entities it was acked
/// on, whatever the allocator's state at replay: bookmarks, edges and suppressions all name them,
/// and recomputing would move a layer under every one of them.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct RegisteredLayer {
    pub declaration: LayerDeclaration,
    /// The layer's own entity. Its only job is to give layer suppression somewhere to land, so
    /// `/control/changes` and the deny lane work on a layer exactly as they work on a point.
    pub entity: crate::EntityId,
    /// One entry per level, in level order; a layer declaring no levels has exactly one, its
    /// level 0.
    pub runs: Vec<ReservedRuns>,
    /// Bumped by any edit that changes who may reach this layer, so a session's cached resolution
    /// is invalidated rather than outliving the gate it was computed from.
    pub version: u64,
}

/// Why a declaration was refused. Every one of these is a fail-closed refusal at registration: the
/// layer does not exist afterwards, and no partial state is left behind.
///
/// Not `Eq`, because [`DeclarationError::FractionOutOfRange`] carries the offending `f64` back to
/// the caller — the number they wrote is what makes the message actionable, and reporting it costs
/// an equality that nothing here needs.
#[derive(Debug, Clone, PartialEq)]
pub enum DeclarationError {
    EmptyName,
    /// A treed layer declaring levels, which is the one combination decision 0082 forbids: its
    /// lineage is in its edges, so a level number would be an address component pretending to carry
    /// position.
    TreeWithLevels,
    /// A stacked layer with no levels — its levels *are* its analyses, so it has declared nothing.
    StackedWithoutLevels,
    /// A tiered layer with no levels. Its edges run *between* levels, so with none declared there
    /// is nowhere for one to run.
    TieredWithoutLevels,
    /// Levels that repeat a number or do not start at 0 and run consecutively. Ordinals are
    /// level-local over a contiguous entity run, so a gap would reserve a run nothing addresses.
    LevelsNotDense,
    /// `min_fraction` outside `(0, 1]`.
    FractionOutOfRange(f64),
    /// A proportional criterion on a predicate layer. ⊘ Refused until the owner rules on the
    /// denominator: *"the points inside this shape"* declares no member set and its size changes at
    /// every write, so the ratio has nothing stable to divide by.
    ProportionalOnPredicate,
    /// A layer naming itself in `depends_on`.
    SelfDependency,
    /// The same view, level title or supplied-content name declared twice.
    Duplicate(String),
    /// A computed property outside [`ComputedProperty::VOCABULARY`].
    ///
    /// **Refused rather than ignored**, and that is a fail-closed choice rather than tidiness: a
    /// served artifact missing content its layer declared is indistinguishable, to a client, from
    /// one whose content was withheld — and nothing is ever withheld from a served artifact
    /// (decision 0076). Accepting an unknown name would put the client in the position of guessing
    /// which of the two it was looking at.
    UnknownComputed(String),
}

impl std::fmt::Display for DeclarationError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            DeclarationError::EmptyName => write!(f, "a layer name may not be empty"),
            DeclarationError::TreeWithLevels => write!(
                f,
                "a nested layer's hierarchy is its edges, so it declares no levels: remove the \
                 levels, or declare the layer stacked if its levels are independent analyses"
            ),
            DeclarationError::StackedWithoutLevels => write!(
                f,
                "a stacked layer's levels are its analyses, so it must declare at least one"
            ),
            DeclarationError::TieredWithoutLevels => write!(
                f,
                "a tiered layer's edges run between its levels, so it must declare them: declare \
                 the levels, or declare the layer nested if its lineage is a tree at one resolution"
            ),
            DeclarationError::LevelsNotDense => write!(
                f,
                "levels must be numbered 0..n consecutively with no repeats — an ordinal is its \
                 entity minus the level's base, so a gap reserves a run nothing addresses"
            ),
            DeclarationError::FractionOutOfRange(p) => {
                write!(f, "require_member_visibility fraction must be in (0, 1]; got {p}")
            }
            DeclarationError::ProportionalOnPredicate => write!(
                f,
                "a proportional criterion needs a declared membership size to divide by, and \
                 predicate membership declares none; use `require_member_visibility = {{ count = n }}` \
                 or `\"none\"`"
            ),
            DeclarationError::SelfDependency => {
                write!(f, "a layer may not name itself in depends_on")
            }
            DeclarationError::Duplicate(what) => write!(f, "declared twice: {what}"),
            DeclarationError::UnknownComputed(name) => write!(
                f,
                "'{name}' is not a computed property this service computes; the vocabulary is {} — \
                 a name outside it is refused rather than ignored, because an artifact served \
                 without content its layer declared cannot be told apart from one whose content \
                 was withheld",
                ComputedProperty::VOCABULARY.join(", ")
            ),
        }
    }
}

impl std::error::Error for DeclarationError {}

impl LayerDeclaration {
    /// Checks the declaration is internally coherent. **Everything here is a refusal a caller can
    /// fix**, checked once at registration rather than at every request — the request-time
    /// invariants (containment, the criterion) are evaluated per request and live elsewhere.
    pub fn validate(&self) -> Result<(), DeclarationError> {
        if self.name.trim().is_empty() {
            return Err(DeclarationError::EmptyName);
        }
        if self.depends_on.iter().any(|d| d == &self.name) {
            return Err(DeclarationError::SelfDependency);
        }

        // **A layer's edges are all within a level or all between levels, never a mix**, and which
        // it is follows from the declared kind rather than from inspecting the edges (§6.2: a layer
        // declares its structure, and it is never inferred from whether edges happen to exist).
        // The publish path and the build both enforce the direction; this is where the shape that
        // makes the question answerable at all is checked.
        match self.hierarchy.kind {
            HierarchyKind::Nested if !self.levels.is_empty() => {
                return Err(DeclarationError::TreeWithLevels)
            }
            HierarchyKind::Stacked if self.levels.is_empty() => {
                return Err(DeclarationError::StackedWithoutLevels)
            }
            HierarchyKind::Tiered if self.levels.is_empty() => {
                return Err(DeclarationError::TieredWithoutLevels)
            }
            _ => {}
        }

        // Levels address by `entity − level.entity_base` over a reserved run, so the set must be
        // exactly 0..n: a repeat would give two levels one base, and a gap would reserve a run no
        // address reaches.
        let mut seen: BTreeSet<u32> = BTreeSet::new();
        for level in &self.levels {
            if !seen.insert(level.level) {
                return Err(DeclarationError::LevelsNotDense);
            }
        }
        if !self.levels.is_empty() && seen.iter().copied().ne(0..self.levels.len() as u32) {
            return Err(DeclarationError::LevelsNotDense);
        }

        if let Some(ExistenceCriterion::Fraction(p)) = self.require_member_visibility {
            if !(p > 0.0 && p <= 1.0) {
                return Err(DeclarationError::FractionOutOfRange(p));
            }
            if matches!(
                self.membership,
                MembershipSource::Spatial | MembershipSource::Attribute
            ) {
                return Err(DeclarationError::ProportionalOnPredicate);
            }
        }

        let mut views: BTreeSet<&str> = BTreeSet::new();
        for view in &self.views {
            if !views.insert(view.as_str()) {
                return Err(DeclarationError::Duplicate(format!("view {view}")));
            }
        }
        let mut computed: BTreeSet<&str> = BTreeSet::new();
        for name in &self.content.computed {
            if ComputedProperty::parse(name).is_none() {
                return Err(DeclarationError::UnknownComputed(name.clone()));
            }
            if !computed.insert(name.as_str()) {
                return Err(DeclarationError::Duplicate(format!(
                    "computed property {name}"
                )));
            }
        }

        let mut names: BTreeSet<&str> = BTreeSet::new();
        for supplied in &self.content.supplied {
            if !names.insert(supplied.name.as_str()) {
                return Err(DeclarationError::Duplicate(format!(
                    "supplied content {}",
                    supplied.name
                )));
            }
        }
        Ok(())
    }

    /// How many reserved entity runs this layer needs: one per declared level, and one for a layer
    /// that declares none — a treed or flat layer still holds its artifacts somewhere, and that
    /// somewhere is level 0.
    pub fn run_count(&self) -> usize {
        self.levels.len().max(1)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn decl(kind: HierarchyKind, levels: Vec<u32>) -> LayerDeclaration {
        LayerDeclaration {
            name: "clusters/x".into(),
            title: Some("X".into()),
            views: vec!["default".into()],
            membership: MembershipSource::Enumerated,
            visibility: None,
            artifact_visibility: ArtifactVisibility::inherited(),
            require_member_visibility: None,
            hierarchy: Hierarchy {
                kind,
                prune_children: false,
            },
            content: ContentDeclaration::default(),
            depends_on: Vec::new(),
            levels: levels
                .into_iter()
                .map(|level| LevelDeclaration {
                    level,
                    title: Some(format!("L{level}")),
                    zoom: None,
                })
                .collect(),
        }
    }

    #[test]
    fn a_tree_declares_no_levels_and_a_stacked_layer_must() {
        // Decision 0082's one forbidden combination, and its mirror. A condensed tree is
        // unbalanced, so a level number would say nothing about position in the lineage.
        assert_eq!(
            decl(HierarchyKind::Nested, vec![0, 1]).validate(),
            Err(DeclarationError::TreeWithLevels)
        );
        assert!(decl(HierarchyKind::Nested, vec![]).validate().is_ok());
        assert_eq!(
            decl(HierarchyKind::Stacked, vec![]).validate(),
            Err(DeclarationError::StackedWithoutLevels)
        );
        assert!(decl(HierarchyKind::Stacked, vec![0, 1, 2]).validate().is_ok());
    }

    #[test]
    fn a_tiered_layer_declares_edges_and_levels_together() {
        // The case the two-name shorthand does not cover: a ward is a ward everywhere, so the
        // resolution is semantic *and* the containment lineage exists. Validation must not force a
        // caller to throw one of them away.
        let mut d = decl(HierarchyKind::Stacked, vec![0, 1, 2]);
        d.membership = MembershipSource::Spatial;
        d.artifact_visibility = ArtifactVisibility::carried("visibility");
        assert!(d.validate().is_ok());
    }

    #[test]
    fn levels_must_be_dense_from_zero() {
        assert_eq!(
            decl(HierarchyKind::Stacked, vec![0, 2]).validate(),
            Err(DeclarationError::LevelsNotDense)
        );
        assert_eq!(
            decl(HierarchyKind::Stacked, vec![1, 2]).validate(),
            Err(DeclarationError::LevelsNotDense)
        );
        assert_eq!(
            decl(HierarchyKind::Stacked, vec![0, 0]).validate(),
            Err(DeclarationError::LevelsNotDense)
        );
    }

    #[test]
    fn a_proportional_criterion_is_refused_on_a_predicate_layer() {
        // ⊘ Until the denominator is ruled: "the points inside this shape" declares no member set,
        // and its size changes at every write, so there is nothing stable to divide by.
        for source in [MembershipSource::Spatial, MembershipSource::Attribute] {
            let mut d = decl(HierarchyKind::Flat, vec![]);
            d.membership = source;
            d.require_member_visibility = Some(ExistenceCriterion::Fraction(0.1));
            assert_eq!(d.validate(), Err(DeclarationError::ProportionalOnPredicate));

            // The absolute form is fine on the same layer — the refusal is about the denominator,
            // not about predicates having no criterion.
            d.require_member_visibility = Some(ExistenceCriterion::Count(50));
            assert!(d.validate().is_ok());
        }

        let mut d = decl(HierarchyKind::Flat, vec![]);
        d.require_member_visibility = Some(ExistenceCriterion::Fraction(1.5));
        assert_eq!(d.validate(), Err(DeclarationError::FractionOutOfRange(1.5)));
        d.require_member_visibility = Some(ExistenceCriterion::Fraction(0.0));
        assert_eq!(d.validate(), Err(DeclarationError::FractionOutOfRange(0.0)));
        d.require_member_visibility = Some(ExistenceCriterion::Fraction(1.0));
        assert!(d.validate().is_ok());
    }

    #[test]
    fn the_two_register_watched_fields_have_no_default() {
        // C27 and C28: the artifact-label declaration and supplied content's membership
        // requirement decide whether a disclosure control runs at all, so a declaration omitting
        // either must fail to parse rather than acquire a value nobody wrote. `deny_unknown_fields`
        // plus the absence of `#[serde(default)]` is what enforces it, and this test is what stops
        // someone adding a default later.
        let complete = serde_json::json!({
            "name": "l", "title": "L", "views": [], "membership": "enumerated",
            "visibility": null,
            "artifact_visibility": { "field": null, "default": "inherited" },
            "require_member_visibility": null,
            "hierarchy": { "kind": "flat" }
        });
        assert!(serde_json::from_value::<LayerDeclaration>(complete).is_ok());

        let missing_artifact_visibility = serde_json::json!({
            "name": "l", "title": "L", "views": [], "membership": "enumerated",
            "visibility": null,
            "require_member_visibility": null,
            "hierarchy": { "kind": "flat" }
        });
        assert!(serde_json::from_value::<LayerDeclaration>(missing_artifact_visibility).is_err());

        let missing_member_default = serde_json::json!({
            "name": "l", "title": "L", "views": [], "membership": "enumerated",
            "visibility": null,
            "artifact_visibility": { "field": null },
            "require_member_visibility": null,
            "hierarchy": { "kind": "flat" }
        });
        assert!(serde_json::from_value::<LayerDeclaration>(missing_member_default).is_err());

        let missing_provenance = serde_json::json!({
            "name": "l", "title": "L", "views": [], "membership": "enumerated",
            "visibility": null,
            "artifact_visibility": { "field": null, "default": "inherited" },
            "require_member_visibility": null,
            "hierarchy": { "kind": "flat" },
            "content": { "supplied": [{ "name": "topic", "type": "text" }] }
        });
        assert!(serde_json::from_value::<LayerDeclaration>(missing_provenance).is_err());
    }

    #[test]
    fn the_two_addressing_directions_agree_across_a_discontiguous_level() {
        // A level extended after other layers took the ids below it: block B sits well under block
        // A, and the ordinals run A then B because that is allocation order. Any scheme that sorted
        // by entity would put B's artifacts first and renumber the level silently.
        let a = EntityRun {
            start: 900 * RESERVED_BLOCK,
            end: 901 * RESERVED_BLOCK,
        };
        let b = EntityRun {
            start: 400 * RESERVED_BLOCK,
            end: 402 * RESERVED_BLOCK,
        };
        let runs = ReservedRuns::from_runs(vec![a, b]);
        assert_eq!(runs.capacity(), 3 * RESERVED_BLOCK);

        for ordinal in [0, 1, RESERVED_BLOCK - 1, RESERVED_BLOCK, 3 * RESERVED_BLOCK - 1] {
            let entity = runs.entity_of(ordinal).expect("within capacity");
            assert_eq!(runs.ordinal_of(entity), Some(ordinal));
        }
        // Ordinal 0 is in the *first allocated* block, not the lowest-addressed one.
        assert_eq!(runs.entity_of(0), Some(900 * RESERVED_BLOCK));
        assert_eq!(runs.entity_of(RESERVED_BLOCK), Some(400 * RESERVED_BLOCK));

        // Past the reservation, and inside neither run.
        assert_eq!(runs.entity_of(3 * RESERVED_BLOCK), None);
        assert_eq!(runs.ordinal_of(500 * RESERVED_BLOCK), None);
    }

    #[test]
    fn one_run_is_the_representations_subtraction() {
        // The common case the design states: `ordinal = entity − entity_base`, with the general
        // form degenerating to exactly that.
        let base = 700 * RESERVED_BLOCK;
        let runs = ReservedRuns::from_runs(vec![EntityRun {
            start: base,
            end: base + RESERVED_BLOCK,
        }]);
        for ordinal in [0, 1, 4_242, RESERVED_BLOCK - 1] {
            assert_eq!(runs.entity_of(ordinal), Some(base + ordinal));
            assert_eq!(runs.ordinal_of(base + ordinal), Some(ordinal));
        }
    }

    #[test]
    fn a_declaration_with_absent_options_round_trips_through_a_positional_encoding() {
        // The guard for the module doc's absolute rule. A `skip_serializing_if` on any `Option`
        // here shortens the postcard record, and every field after it decodes from the wrong bytes:
        // a layer replays with someone else's gate, or the record fails to parse at all. This test
        // is what turns re-adding one from a silent corruption into a red build.
        //
        // `postcard` is not a dependency of this crate, so the check is done with the property that
        // matters — a *positional* encoding, where absence cannot be signalled — using serde's own
        // tuple form via `serde_json` on a sequence.
        let mut d = decl(HierarchyKind::Flat, vec![]);
        d.visibility = None;
        d.require_member_visibility = None;
        d.levels = vec![LevelDeclaration {
            level: 0,
            title: Some("only".into()),
            zoom: None,
        }];
        d.hierarchy.kind = HierarchyKind::Stacked;

        let json = serde_json::to_value(&d).unwrap();
        let object = json.as_object().expect("a struct serialises as an object");
        // Every `Option` in the whole shape, at each nesting level it appears.
        assert!(
            object.contains_key("require_member_visibility"),
            "require_member_visibility was omitted — a positional encoding cannot express that, so \
             every field after it would decode from the wrong bytes"
        );
        assert!(object.contains_key("visibility"));
        assert!(object["artifact_visibility"]
            .as_object()
            .unwrap()
            .contains_key("field"));
        assert!(object["levels"][0]
            .as_object()
            .unwrap()
            .contains_key("zoom"));

        assert_eq!(serde_json::from_value::<LayerDeclaration>(json).unwrap(), d);
    }

    #[test]
    fn run_count_is_one_for_a_layer_that_declares_no_levels() {
        // A treed layer still holds its artifacts somewhere, and that somewhere is level 0 — so it
        // reserves one run, not zero.
        assert_eq!(decl(HierarchyKind::Nested, vec![]).run_count(), 1);
        assert_eq!(decl(HierarchyKind::Flat, vec![]).run_count(), 1);
        assert_eq!(decl(HierarchyKind::Stacked, vec![0, 1, 2]).run_count(), 3);
    }
}
