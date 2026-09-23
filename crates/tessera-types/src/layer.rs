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
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum MembershipSource {
    /// A stored set of entities per artifact. Stale between the write and the refresh that rebuilds
    /// it, which is fail-closed: an unrebuilt member has no bit, so every masked count understates.
    Enumerated,
    /// A shape — *the rows whose stored position is inside it*, exactly, for every kind
    /// (`polygon-membership.md` §4.1). Never stale: a segment's rows are resolved against the
    /// shapes when the segment is published, so a point ingested inside a boundary is a member on
    /// the next request with nothing rebuilt.
    ///
    /// **What the shape *is* lives beside this, on [`LayerDeclaration::shape`]**, and a layer that
    /// declares none holds no artifacts. The two are separate fields because this one says *what
    /// invalidates a write* and that one says *what kind of geometry each artifact carries*.
    Spatial,
    /// A predicate over an existing value column, which the variant names: the membership is
    /// defined by that field's value, so the field is part of the declaration rather than
    /// something a reader could infer. Never stale, for the same reason `Spatial` is not.
    Attribute(String),
}

/// How a level's membership is **stored and scanned** at serving time
/// ([decision 0094](../../../docs/decisions/0094-the-serving-layout-is-chosen-at-build-and-re-evaluated-at-the-fold.md)).
///
/// **Not a contract, and nothing on the wire names one.** Both forms answer identically — the same
/// served set, the same counts, the same ranks and parents — so no request field selects one, no
/// response reports one, and a fold may change one freely. What it decides is what a request
/// *costs*: an artifact-major level is walked through the tile index and probed per artifact; a
/// row-major level is one scan of `viewport ∩ M_auth` over a column addressed by **row**.
///
/// **Recorded per `(layer, level)`** on [`RegisteredLayer::layouts`], because the levels differ: a
/// treed layer's coarse level holds ten thousand nodes and its leaf level ten million, and one
/// record for the layer would average two different problems. The **pin** on
/// [`LayerDeclaration::layout`] is per *layer*, because that is where a declaration lives.
///
/// **The label/list split follows from the membership**, not from a preference: a level whose
/// memberships are disjoint has exactly one label per row, and one whose memberships overlap does
/// not. A level pinned [`RowMajorLabel`](ServingLayout::RowMajorLabel) whose memberships turn out to
/// overlap is composed **artifact-major**, loudly — see `tessera_engine::layout`.
///
/// **A spatial level takes one of the same three forms.** Its membership is resolved from the
/// shapes when a segment is published (`polygon-membership.md` §6.3), and the output is a per-row
/// source exactly as an enumerated level's member table is — so the pick below chooses between the
/// same forms for it, and a pin selects between them.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ServingLayout {
    /// One row-space bitmap per artifact — what the engine has always built. Candidacy is the tile
    /// index's walk, then the extent test, then the composed probe at the viewport's edge; the
    /// count is `|membership ∩ M_auth|` per served artifact.
    ///
    /// **The default, and deliberately so.** It is the form every derived structure already exists
    /// for, and the automatic pick is conservative in its direction (`tessera_engine::layout`).
    #[default]
    ArtifactMajor,
    /// One artifact label per **row**, for a level whose memberships partition the corpus.
    /// Candidacy is one scan of `viewport ∩ M_auth` marking labels; the count comes from the
    /// per-`(session, layer)` masked-count histogram
    /// ([decision 0093](../../../docs/decisions/0093-nothing-is-materialised-per-token-over-the-artifact-population.md)'s
    /// one named exception).
    RowMajorLabel,
    /// A **list** of labels per row, for a level whose memberships overlap. The same scan and the
    /// same histogram at a larger constant.
    RowMajorList,
}

impl ServingLayout {
    /// Whether this layout is scanned by row rather than probed by artifact — the one question the
    /// serving path asks of it.
    pub fn is_row_major(self) -> bool {
        matches!(
            self,
            ServingLayout::RowMajorLabel | ServingLayout::RowMajorList
        )
    }

    /// The word a `[[layer]]` block spells this layout with (`configuration.md` §1).
    ///
    /// **Three words for three variants**, rather than the two-word `row-major` family the selection
    /// memo proposed: an operator pinning a level has a reason, and `column` against `list` is the
    /// difference between *I assert this partitions* and *I assert it does not*. The first is
    /// checkable at the fold and falls back loudly when it is wrong, which is what makes stating it
    /// worth more than having it inferred.
    pub fn pin_word(self) -> &'static str {
        match self {
            ServingLayout::ArtifactMajor => "rows",
            ServingLayout::RowMajorLabel => "column",
            ServingLayout::RowMajorList => "list",
        }
    }

    /// The layout a `layout = "…"` key names, or `None` for a word outside the vocabulary.
    pub fn parse_pin(word: &str) -> Option<Self> {
        match word {
            "rows" => Some(ServingLayout::ArtifactMajor),
            "column" => Some(ServingLayout::RowMajorLabel),
            "list" => Some(ServingLayout::RowMajorList),
            _ => None,
        }
    }

    /// Every word a `layout` key may carry.
    pub const PIN_VOCABULARY: [&'static str; 3] = ["rows", "column", "list"];
}

/// Whether a key nothing declares is refused, or creates the object it names
/// (`artifacts-from-points.md` §3, `per-point-attributes.md` §3.4).
///
/// **Closed**: an unknown key is refused — declare-then-use. On a layer that is the roster rule a
/// member source has always had: a mistyped id would otherwise publish a phantom artifact carrying
/// the members it stole from a real one, whose masked count then goes quietly short.
///
/// **Open**: an unknown key creates the object, carrying nothing but its name. On a layer that is
/// *a cluster exists because points say it does*: the artifacts source becomes enrichment — titles,
/// parents, contents for the clusters somebody knows something about — rather than the roster, and
/// a cluster it omits exists without a title.
///
/// One type for a layer and for a vocabulary because it is one question, asked of two objects that
/// both mint identities from keys arriving in data.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ValueSet {
    #[default]
    Closed,
    Open,
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
    /// ([`ArtifactVisibility::carries_own_labels`]). Naming a field therefore declares the shape without yet
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
    pub fn carries_own_labels(&self) -> bool {
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
    /// A directed acyclic graph, held in the layer's edges — [`Nested`](HierarchyKind::Nested) in
    /// every respect but one: **a child may name several parents**, and a second parent arriving
    /// for a child is recorded rather than refused (`dag-hierarchies.md` §3, decision 0117). Every
    /// artifact sits at level 0, `[[layer.levels]]` is refused, the edges are roll-up, and a budget
    /// climbs the edges. A self-edge and a cycle refuse at both entry points.
    ///
    /// **Its edges are spelled on the artifact row's `parent` list and nowhere else** (decision
    /// 0125). A list key column under `dag` is plain multi-membership, read exactly as `flat`
    /// reads one: a tree node's ancestor closure is a chain, so a `nested` lineage list states
    /// memberships and edges at once; a DAG node's closure is a set with no linear order, so the
    /// adjacency of its list carries nothing anyone could have meant.
    ///
    /// A kind value rather than a key on `nested`, because the kind is what every reader switches
    /// on and a tree and a graph are different shapes; and `nested`'s cousin rather than
    /// `tiered`'s, because a concept at several depths cannot be placed at one level.
    Dag,
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
    /// What it is — `text`, `extent`, `point`, or one of the three **authored shape** kinds
    /// `polygon`, `circle`, `ellipse` ([`Self::authored_shape_kind`]). Published in `/v1/meta` so
    /// a client knows what to draw; publishing the *types* is safe because an artifact failing
    /// containment is absent whole, so no served artifact ever lacks a content its layer declares.
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

impl SuppliedContent {
    /// The shape kind this content authors, where its `type` is one of the three shape words —
    /// `polygon`, `circle`, `ellipse` (`polygon-membership.md` §6.1, ruling (h)). Such a content
    /// is read at publication as a membership shape is, stored canonical beside the artifact's
    /// other content, and served as the layer's **authored** drawn geometry through `shape_x` /
    /// `shape_y`; it selects nothing. `None` for every other type, which is carried as text.
    pub fn authored_shape_kind(&self) -> Option<ShapeKind> {
        match self.ty.as_str() {
            "polygon" => Some(ShapeKind::Polygon),
            "circle" => Some(ShapeKind::Circle),
            "ellipse" => Some(ShapeKind::Ellipse),
            _ => None,
        }
    }
}

impl SuppliedRequirement {
    /// Whether this content requires **every** member of its generating set to be visible — the
    /// `"all"` setting, and the reason such content must arrive with a generating set at all.
    pub fn requires_all_members(self) -> bool {
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
    /// The hull of the visible members — a concave (alpha) shape, tightened from the convex wrap.
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

    /// **The ask vocabulary** — what a `/v1/viewport` request's `computed` may name
    /// (`polygon-membership.md` §7.1): the same three words with `shape` in place of `hull`. A
    /// layer has one drawn geometry of a declared kind — derived (the hull), predicate (the
    /// membership shape) or authored (a supplied drawing) — and a request asks for *the shape*
    /// without knowing which; the declaration keeps the word `hull` because that is what an
    /// enumerated layer computes. The narrowing rule is unchanged: a layer with no drawn geometry
    /// serves none however it is asked.
    pub const ASK_VOCABULARY: [&'static str; 3] = ["centroid", "box", "shape"];

    /// Parse a request's `computed` word: `shape` selects the layer's drawn geometry, which for
    /// a derived layer is the [`ComputedProperty::Hull`] it declared. `hull` is **not** an ask
    /// word — the request names the drawing, not its derivation.
    pub fn parse_ask(name: &str) -> Option<Self> {
        match name {
            "centroid" => Some(ComputedProperty::Centroid),
            "box" => Some(ComputedProperty::Box),
            "shape" => Some(ComputedProperty::Hull),
            _ => None,
        }
    }
}

/// Which of the three kinds a layer's **one drawn geometry** is (`polygon-membership.md` §7.1,
/// owner ruling 2026-08-29). Published per layer in `/v1/meta` as `shape`, so a client knows
/// whether the outline moves with the principal — which decides whether it may hold the geometry
/// against a `tessera_id` across principals.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum DrawnShape {
    /// The hull over the visible members — `content.computed` names `hull` — recomputed per
    /// principal from `membership ∩ M_auth`, so two principals receive two drawings.
    Derived,
    /// The membership shape of a `spatial` layer, served under the artifact's own verdict and
    /// identical for every principal served the artifact.
    Predicate,
    /// A supplied `polygon`, `circle` or `ellipse` content over an enumerated or attribute
    /// membership — a fitted circle over a k-means cluster (`annotations.md` §8.6) — gated by
    /// that content's own `require_member_visibility`, identical for every principal served it.
    Authored,
}

impl DrawnShape {
    pub fn name(self) -> &'static str {
        match self {
            DrawnShape::Derived => "derived",
            DrawnShape::Predicate => "predicate",
            DrawnShape::Authored => "authored",
        }
    }
}

/// What a layer's artifacts carry.
///
/// A deleted member withdraws the supplied content its generating set produced (decision 0135).
/// The item leaves every mask, so the set fails containment for every principal; the fold removes
/// the content, reports it, and the caller re-declares the set or the content. No field selects
/// another outcome: the strict and permissive modes, and the `withdraw_on_member_deletion` field
/// that chose between them, are gone. A declaration still carrying that field is refused by name
/// rather than read with it ignored (decision 0048); see the `Deserialize` impl below.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize)]
pub struct ContentDeclaration {
    /// Properties recomputed per viewer from `membership ∩ M_auth` and nothing else — `centroid`,
    /// `hull`, `box`, `extractive_terms`. Contained by construction, so they take no visibility
    /// declaration and pass containment automatically. The masked count is intrinsic and is never
    /// declared here.
    #[serde(default)]
    pub computed: Vec<String>,
    #[serde(default)]
    pub supplied: Vec<SuppliedContent>,
}

/// The refusal a content declaration carrying the removed field draws.
pub const WITHDRAW_ON_MEMBER_DELETION_REMOVED: &str = "`withdraw_on_member_deletion` was removed \
    from the content declaration (decision 0135): a deleted member withdraws the content its \
    generating set produced, at the fold, and the caller re-declares the set or the content. \
    Remove the field";

/// Hand-written so the removed field is refused by name. `deny_unknown_fields` would refuse it as
/// one unknown key among any; a caller holding a declaration written for the old modes should
/// read what changed.
///
/// The check runs only from a self-describing format (`is_human_readable`, which is JSON at the
/// control plane). Postcard is positional and carries the two live fields alone, so the WAL's
/// `LayerCreate` record is read through the positional form.
impl<'de> Deserialize<'de> for ContentDeclaration {
    fn deserialize<D: serde::Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        const FIELDS: &[&str] = &["computed", "supplied"];

        #[derive(Deserialize)]
        #[serde(deny_unknown_fields)]
        struct Positional {
            #[serde(default)]
            computed: Vec<String>,
            #[serde(default)]
            supplied: Vec<SuppliedContent>,
        }

        #[derive(Deserialize)]
        struct Named {
            #[serde(default)]
            computed: Vec<String>,
            #[serde(default)]
            supplied: Vec<SuppliedContent>,
            #[serde(flatten)]
            other: std::collections::BTreeMap<String, serde::de::IgnoredAny>,
        }

        if !deserializer.is_human_readable() {
            let read = Positional::deserialize(deserializer)?;
            return Ok(ContentDeclaration {
                computed: read.computed,
                supplied: read.supplied,
            });
        }
        let read = Named::deserialize(deserializer)?;
        if read.other.contains_key("withdraw_on_member_deletion") {
            return Err(serde::de::Error::custom(
                WITHDRAW_ON_MEMBER_DELETION_REMOVED,
            ));
        }
        if let Some(unknown) = read.other.keys().next() {
            return Err(serde::de::Error::unknown_field(unknown, FIELDS));
        }
        Ok(ContentDeclaration {
            computed: read.computed,
            supplied: read.supplied,
        })
    }
}

/// The one word a viewport request's `layers` field may carry in place of a list: every layer
/// the principal reaches. A layer may not be registered under it ([`DeclarationError::ReservedName`]).
pub const RESERVED_LAYER_SELECTION: &str = "all";

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
    /// Min/max zoom, as every tile schema carries — **and, since 2026-08-28, the default bound on a
    /// levelled layer's response**.
    ///
    /// A `/v1/viewport` request that names no `levels` is answered at the levels whose range covers
    /// the depth it asked at; one that names them overrides this entirely. A layer where no level
    /// declares a range is unaffected and serves every level, which is what keeps this inert on a
    /// treed layer (which declares no levels at all) and on any layer whose author declared none.
    ///
    /// **It was advisory and bounded nothing**, published in `/v1/meta` for a client to follow with
    /// no way to act on it: the request carried the same 0–16 depth coordinate and nothing joined
    /// the two, so a five-level administrative hierarchy was served whole at every zoom and a client
    /// following the published map paid for five levels and drew one. What bounds a **treed**
    /// layer's response is still the request's artifact budget; it has no levels for this to reach.
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
    /// Whether a member key no artifact declares is refused, or creates one
    /// (`artifacts-from-points.md` §3). Defaults to [`ValueSet::Closed`], which is the roster rule
    /// the build has always had.
    ///
    /// **On the layer rather than on the acquisition block**, because ingest has no member block: a
    /// key living in a build-only block could not govern what the write path does with an unknown
    /// id, and governing both entry points is the point of it — and it now does. A build mints from
    /// a member source; an ingest batch mints from a column named for the layer, at the close of the
    /// commit window that allocates the points, carrying them as the new artifact's membership
    /// ([decision 0091](../../../docs/decisions/0091-build-is-ingest-into-an-empty-database.md) is
    /// discharged: the two entry points say the same things).
    ///
    /// **What `open` costs is that a typo is no longer a refusal.** A mistyped key creates a
    /// permanent object rather than failing, which is the trade the declaration makes knowingly; the
    /// mitigation is that the number is reported — in the build's own report, and in the 200 that
    /// accepted the batch.
    #[serde(default)]
    pub value_set: ValueSet,
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
    // The content-level key of the same name was removed by decision 0135; a declaration still
    // carrying it is refused by name (`ContentDeclaration`'s `Deserialize`).
    /// The layers this one's edges point into. A layer named here needs keys, because an
    /// edge names its target and at publish time the caller has no `tessera_id` for it.
    #[serde(default)]
    pub depends_on: Vec<String>,
    /// Empty for a treed or flat layer.
    #[serde(default)]
    pub levels: Vec<LevelDeclaration>,
    /// **The layout pin**: serve every level of this layer in the named form, at the build and at
    /// every fold after it. `None` — the automatic pick, which is the normal state.
    ///
    /// **`#[serde(default)]`, and that is not the register's exception being taken lightly.** The
    /// two fields with no default are disclosure controls whose absent value would be a grant (C27,
    /// C28). A layout is neither: both forms compute the same quantities from inside `M_auth`, no
    /// request field names one and no response reports one, so a declaration that omits this is a
    /// declaration that has no opinion about storage — which is a complete statement rather than an
    /// unfilled one.
    ///
    /// **An override a fold could overturn is not an override** (decision 0094). A pinned layer is
    /// rebuilt in its declared form at every fold, and the observations the automatic pick *would*
    /// have read are recorded beside it so an operator can see what they were.
    #[serde(default)]
    pub layout: Option<ServingLayout>,
    /// **What kind of shape a `membership = "spatial"` layer's artifacts carry.**
    ///
    /// `None` on every other membership source, and refused there. `None` on a spatial layer is the
    /// state that has always existed — a layer declared for a shape it does not yet carry, which
    /// holds no artifacts because publication into it is refused.
    ///
    /// **`#[serde(default)]` on [`LayerDeclaration::layout`]'s argument, and it is not a
    /// disclosure control.** The membership is `spatial` either way; what this adds is the shape,
    /// and a layer without it serves nothing rather than serving something wider.
    #[serde(default)]
    pub shape: Option<ShapeDeclaration>,
    /// **Which artifact set this layer carries over the views it names** (`views.md` §3.5,
    /// [decision 0109](../../../docs/decisions/0109-scope-binds-an-attribute-or-layer-to-a-groups-views.md)):
    /// [`LayerScope::Entity`] — the default — is one set drawn on every view; a group scope is a
    /// different set per view of that group, each artifact belonging to one.
    ///
    /// **On the declaration, so the manifest carries it** (contracts §2.3): the scope was compiled
    /// beside the declaration and written nowhere, so a bundle reopened without its build
    /// configuration could not tell the two kinds of layer apart — and the two answer differently
    /// on every view. `#[serde(default)]` is entity scope, which is a complete statement and not
    /// an unfilled one: it says *one set, every view*, which is what a layer that mentions no
    /// group means.
    #[serde(default)]
    pub scope: LayerScope,
}

/// What a layer's artifacts are per (`views.md` §3.5).
///
/// Spelled as an attribute's scope is (`configuration.md`): `scope = "entity"` and
/// `scope = { group = "quarter" }`.
#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum LayerScope {
    /// One artifact set, drawn on every view the layer names.
    #[default]
    Entity,
    /// A different artifact set per view of the named group. The artifact rows carry a `view`
    /// column, keys are unique per `(layer, view)`, and edges may not cross views.
    Group(String),
}

impl LayerScope {
    /// The group this layer's artifact sets are per, or `None` for the entity-scoped default.
    pub fn group(&self) -> Option<&str> {
        match self {
            LayerScope::Entity => None,
            LayerScope::Group(group) => Some(group.as_str()),
        }
    }
}

/// What kind of shape a spatial layer's artifacts carry (`polygon-membership.md` §6.1).
///
/// **The kind and nothing else.** Every kind is exact — the members are the rows whose stored
/// position is inside the shape — so there is no depth, no tolerance and no cover for the
/// declaration to hold; the geometry itself sits on each artifact's own row in the kind's fields.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ShapeDeclaration {
    pub kind: ShapeKind,
}

/// The four kinds a [`ShapeDeclaration`] may name, with one semantics (`polygon-membership.md`
/// §4.1): *the rows whose stored position is inside the shape*.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ShapeKind {
    /// An axis-aligned box, closed on every side: `bbox = [min_x, min_y, max_x, max_y]`.
    #[default]
    Bbox,
    /// `circle = [cx, cy, r]`.
    Circle,
    /// `ellipse = [cx, cy, a, b, angle]`, the angle in degrees anticlockwise from the x axis.
    Ellipse,
    /// An OGC `MultiPolygon` under the even-odd rule with a point on an edge inside — WKB in a
    /// table's `geometry` column, WKT inline.
    Polygon,
}

impl ShapeKind {
    /// The word a declaration writes and `disclosure.json`'s `spatial:<kind>` spells.
    pub fn as_str(self) -> &'static str {
        match self {
            ShapeKind::Bbox => "bbox",
            ShapeKind::Circle => "circle",
            ShapeKind::Ellipse => "ellipse",
            ShapeKind::Polygon => "polygon",
        }
    }

    pub fn parse(word: &str) -> Option<Self> {
        match word {
            "bbox" => Some(ShapeKind::Bbox),
            "circle" => Some(ShapeKind::Circle),
            "ellipse" => Some(ShapeKind::Ellipse),
            "polygon" => Some(ShapeKind::Polygon),
            _ => None,
        }
    }

    pub const VOCABULARY: [&'static str; 4] = ["bbox", "circle", "ellipse", "polygon"];
}

/// The vertex cap a published polygon is held to, absent a deployment's own
/// (`polygon-membership.md` §9, ruling (e)): one million. A shape over it is refused at
/// publication naming the count and the cap — a vertex count is the caller's own arithmetic and
/// `ST_Simplify` is the fix — where the held decomposition is reported and never capped.
pub const DEFAULT_MAX_SHAPE_VERTICES: u64 = 1_000_000;

/// **The artifact key a `membership = { attribute = f }` layer mints for one value of `f`.**
///
/// One rule read twice. A **category**'s value has a key the author wrote — `finance`, `amber` —
/// and that key is what the value *is*, so it is what the artifact is named; a **plain** integer
/// column has no vocabulary and its value is the number, so the key is that number's canonical
/// decimal spelling. Passing the vocabulary's key or `None` is therefore not a choice at the call
/// site: it is whether the column has a vocabulary.
///
/// **Canonical decimal, so a key is a function of the value and not of who spelled it.** The two
/// entry points mint from different places — a build from the column it has just read, an ingest
/// from the row that has just arrived — and a key that could be written two ways would let one of
/// them mint a second artifact for a value the other already named.
pub fn attribute_value_key(code: u32, vocabulary_key: Option<&str>) -> String {
    match vocabulary_key {
        Some(key) => key.to_string(),
        None => code.to_string(),
    }
}

/// The deepest tile a viewport request may ask at, and so the deepest a level's `zoom` range can
/// usefully name.
///
/// **Sixteen, because that is where the code space ends.** A Morton code interleaves two 16-bit
/// cell coordinates (`tessera_spatial::interleave_bits`), so a depth-16 tile is one cell and a
/// deeper one names a subdivision the geometry cannot express.
pub const MAX_TILE_DEPTH: u32 = 16;

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
        debug_assert_eq!(
            run.start % RESERVED_BLOCK,
            0,
            "reserved runs are block-aligned"
        );
        debug_assert_eq!(
            run.len() % RESERVED_BLOCK,
            0,
            "reserved runs are whole blocks"
        );
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
    /// The serving layout **per level**, parallel to [`RegisteredLayer::runs`] — decision 0094's
    /// record.
    ///
    /// Set at registration from the pin, or [`ServingLayout::ArtifactMajor`] where there is none: a
    /// level with no artifacts has no shape to observe, and the conservative pick is the form every
    /// derived structure already exists for. **Re-evaluated inside every fold's artifact pass**,
    /// before the registry snapshot the manifest is written from, so the record and the files the
    /// same fold wrote cannot disagree.
    ///
    /// **A flip does not bump [`RegisteredLayer::version`]** (selection memo §5). That version gates
    /// reachability and is a fail-closed guard against a reader holding a stale idea of a layer;
    /// a layout is not a client-visible fact, so bumping it would make every session re-resolve a
    /// layer for a change none of them can observe.
    ///
    /// Shorter than `runs` is read as [`ServingLayout::ArtifactMajor`] for the levels past its end
    /// — see [`RegisteredLayer::layout_of`] — which is the fail-safe direction: the worst outcome
    /// is a row-major column nothing adopts, and the level is served the way it always was.
    pub layouts: Vec<ServingLayout>,
}

impl RegisteredLayer {
    /// The layout recorded for one level. Absent is [`ServingLayout::ArtifactMajor`] — see
    /// [`RegisteredLayer::layouts`].
    pub fn layout_of(&self, level: u32) -> ServingLayout {
        self.layouts
            .get(level as usize)
            .copied()
            .unwrap_or_default()
    }

    /// The record every level of a freshly registered layer starts at: the form the membership
    /// forces where it forces one, the pin where there is one, and artifact-major otherwise.
    ///
    /// **An attribute layer's form is not a pick and not a pin**: its membership *is* the column,
    /// so there is no alternative to be chosen between — which is why `validate` refuses a pin on
    /// it and why the fold's re-evaluation leaves it alone. A spatial layer's membership is
    /// resolved into a per-row source at every publication of a segment, so it is picked and
    /// pinned exactly as an enumerated layer's is.
    pub fn initial_layouts(declaration: &LayerDeclaration) -> Vec<ServingLayout> {
        let forced = match declaration.membership {
            // **The membership is the column** (`design/artifact-serving-at-scale.md` §5.1): a
            // single-valued attribute partitions the corpus, so one label per row is the only form
            // its membership has — there is no per-artifact bitmap to fall back to.
            MembershipSource::Attribute(_) => Some(ServingLayout::RowMajorLabel),
            MembershipSource::Spatial | MembershipSource::Enumerated => None,
        };
        vec![forced.or(declaration.layout).unwrap_or_default(); declaration.run_count()]
    }
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
    /// A treed layer — `nested` or `dag` — declaring levels, which is the one combination decision
    /// 0082 forbids: its lineage is in its edges, so a level number would be an address component
    /// pretending to carry position.
    TreeWithLevels,
    /// A stacked layer with no levels — its levels *are* its analyses, so it has declared nothing.
    StackedWithoutLevels,
    /// A tiered layer with no levels. Its edges run *between* levels, so with none declared there
    /// is nowhere for one to run.
    TieredWithoutLevels,
    /// Levels that repeat a number or do not start at 0 and run consecutively. Ordinals are
    /// level-local over a contiguous entity run, so a gap would reserve a run nothing addresses.
    LevelsNotDense,
    /// `require_member_visibility = { fraction = p }` with `p` outside `(0, 1]`.
    FractionOutOfRange(f64),
    /// A proportional criterion on a predicate layer. ⊘ Refused until the owner rules on the
    /// denominator: *"the points inside this shape"* declares no member set and its size changes at
    /// every write, so the ratio has nothing stable to divide by — and keeping it refused is what
    /// keeps a shape's declared size off the wire (`polygon-membership.md` §10).
    ProportionalOnPredicate,
    /// A level's `zoom` range has no depth in it — its ends are inverted, or it starts past the
    /// grid's own depth of 16. Refused rather than warned because it has no reading at all: since
    /// the range became the default bound on a response (decision 0103) such a level is served at
    /// no depth, and the operator's only symptom would be a layer that is silently absent.
    ZoomRangeEmpty {
        level: u32,
        lo: u32,
        hi: u32,
    },
    /// A layer declares a `shape` and its membership is not `spatial`, so nothing would read it.
    ShapeWithoutSpatialMembership,
    /// A layer declares two drawn geometries — a derived hull, a membership shape and an authored
    /// shape content are the three kinds, and an artifact has one (`polygon-membership.md` §7.1).
    /// Carries the two spellings, so the message names what to remove.
    TwoDrawnGeometries(String),
    /// An attribute layer declares something its derived artifacts cannot carry — content, a
    /// dependency, levels, its own access labels, or a layout pin. Carries the spelling, so the
    /// message names the key an operator has to remove.
    PredicateDeclares(String),
    /// A layer's `visibility`, `artifact_visibility.default` or `artifact_visibility.field` is not
    /// a word that can be read. Carries the refusal.
    Label(String),
    /// A layer naming itself in `depends_on`.
    SelfDependency,
    /// A layer named `all`, which the viewport request's `layers` field reserves for *every layer
    /// this principal reaches* (contracts §3.2; owner ruling 2026-08-25). Refused at registration
    /// so the word can never be ambiguous on the wire.
    ReservedName(String),
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
            DeclarationError::ReservedName(name) => write!(
                f,
                "'{name}' is reserved: a viewport request's `layers: \"{RESERVED_LAYER_SELECTION}\"` \
                 names every layer the principal reaches, so no layer may carry that name"
            ),
            DeclarationError::TreeWithLevels => write!(
                f,
                "a nested or dag layer's hierarchy is its edges, so it declares no levels: remove \
                 the levels, or declare the layer stacked if its levels are independent analyses"
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
            DeclarationError::ZoomRangeEmpty { level, lo, hi } => write!(
                f,
                "level {level} declares zoom [{lo}, {hi}], which contains no depth: the ends are \
                 inverted or the range starts past the grid's own depth of {MAX_TILE_DEPTH}. A \
                 request naming no `levels` is answered at the levels whose range covers its depth, \
                 so this level would be served at none. Omit `zoom` to serve it at every depth"
            ),
            DeclarationError::ShapeWithoutSpatialMembership => write!(
                f,
                "a layer declares a `shape` and its `membership` is not `spatial`, so the shape is \
                 a rule nothing evaluates — the members come from the stored set or the predicate \
                 the membership names, and the box beside them would decide nothing"
            ),
            DeclarationError::TwoDrawnGeometries(what) => write!(
                f,
                "a layer declares {what}; an artifact has one drawn geometry, served through one \
                 `shape_x`/`shape_y` column pair, so a layer declares at most one of a derived \
                 hull, a membership shape and an authored shape content"
            ),
            DeclarationError::Label(detail) => write!(f, "{detail}"),
            DeclarationError::PredicateDeclares(what) => write!(
                f,
                "a layer whose membership is a predicate declares {what}, which its artifacts \
                 cannot carry: they are derived from the rule, not published with properties \
                 beside them. A layer registered with this would be reachable and serve nothing, \
                 which no client can tell from one whose artifacts were all withheld"
            ),
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
    /// The authored shape content, where the layer declares one: its position among the supplied
    /// kinds — the slot its value occupies in every ranked content and in the wire's `content`
    /// list — and the kind it authors. [`Self::validate`] refuses a second.
    pub fn authored_shape(&self) -> Option<(usize, ShapeKind)> {
        self.content
            .supplied
            .iter()
            .enumerate()
            .find_map(|(k, s)| s.authored_shape_kind().map(|kind| (k, kind)))
    }

    /// Which kind the layer's one drawn geometry is, or `None` where it draws nothing but its
    /// centroid and box. Well-defined because [`Self::validate`] refuses two.
    pub fn drawn_shape(&self) -> Option<DrawnShape> {
        if self.content.computed.iter().any(|c| c == "hull") {
            Some(DrawnShape::Derived)
        } else if self.membership == MembershipSource::Spatial && self.shape.is_some() {
            Some(DrawnShape::Predicate)
        } else if self.authored_shape().is_some() {
            Some(DrawnShape::Authored)
        } else {
            None
        }
    }

    pub fn validate(&self) -> Result<(), DeclarationError> {
        if self.name.trim().is_empty() {
            return Err(DeclarationError::EmptyName);
        }
        if self
            .name
            .trim()
            .eq_ignore_ascii_case(RESERVED_LAYER_SELECTION)
        {
            return Err(DeclarationError::ReservedName(self.name.clone()));
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
            HierarchyKind::Nested | HierarchyKind::Dag if !self.levels.is_empty() => {
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

        // **A zoom range must contain a depth**, because since 2026-08-28 it decides what a request
        // naming no `levels` is answered at (decision 0103). While the range was advisory an
        // inverted or out-of-grid one was harmless; now it means the level is served at no depth,
        // and a layer that quietly vanishes at every zoom is the least diagnosable failure this
        // surface can produce. `zoom` being absent is a different thing and stays legal: it means
        // *served at every depth*.
        //
        // A **gap** between two levels' ranges is not refused — a declaration may legitimately have
        // no level for some band — but the build prints every range beside its level so a gap is
        // visible rather than inferred (`tessera_build::artifact_pass::report`).
        for level in &self.levels {
            if let Some((lo, hi)) = level.zoom {
                if lo > hi || lo > MAX_TILE_DEPTH {
                    return Err(DeclarationError::ZoomRangeEmpty {
                        level: level.level,
                        lo,
                        hi,
                    });
                }
            }
        }

        if let Some(ExistenceCriterion::Fraction(p)) = self.require_member_visibility {
            if !(p > 0.0 && p <= 1.0) {
                return Err(DeclarationError::FractionOutOfRange(p));
            }
            if matches!(
                self.membership,
                MembershipSource::Spatial | MembershipSource::Attribute(_)
            ) {
                return Err(DeclarationError::ProportionalOnPredicate);
            }
        }

        // **The shape declaration and the membership are one statement in two fields**, and each
        // half without the other is a declaration that cannot serve: a `shape` on a layer whose
        // members are a stored set or a predicate is a rule nothing reads.
        //
        // ⊘ A spatial layer with no `shape` is the state this surface has always had: declared,
        // registered, and holding nothing, because publication into it is refused. It stays
        // expressible rather than becoming a refusal — it is what a fixture declares while the
        // shape it will carry is still being written — and the *build* is where it is reported,
        // beside the artifacts it would have had.
        if self.shape.is_some() && self.membership != MembershipSource::Spatial {
            return Err(DeclarationError::ShapeWithoutSpatialMembership);
        }

        // **One drawn geometry per layer** (`polygon-membership.md` §7.1, owner ruling
        // 2026-08-29): the wire carries one `shape_x`/`shape_y` pair per artifact and `/v1/meta`
        // publishes one kind per layer, so a layer that could draw two — a hull beside a
        // membership shape, an authored polygon beside either, two authored kinds — has no
        // column for the second and is refused naming both.
        let mut drawn: Vec<String> = Vec::new();
        if self.content.computed.iter().any(|c| c == "hull") {
            drawn.push("a derived `hull`".to_string());
        }
        if self.membership == MembershipSource::Spatial && self.shape.is_some() {
            drawn.push("the membership shape of a `spatial` layer".to_string());
        }
        for supplied in &self.content.supplied {
            if supplied.authored_shape_kind().is_some() {
                drawn.push(format!(
                    "the authored `{}` content '{}'",
                    supplied.ty, supplied.name
                ));
            }
        }
        if drawn.len() > 1 {
            return Err(DeclarationError::TwoDrawnGeometries(drawn.join(" and ")));
        }

        // **What an attribute layer may not declare, and why each one is a refusal rather than a
        // warning.** Every item here would leave the layer registered, reachable and serving
        // nothing — which is exactly the state the build already refuses for a layer declared in a
        // view it does not write, and which no client can tell from a layer whose artifacts were
        // all withheld. Its membership is a rule over a column, so the artifacts it names are the
        // column's distinct values and carry their key and nothing else.
        //
        // **A spatial layer is not an attribute layer, and the refusals do not reach it**
        // (`polygon-membership.md` §6.2, ruling (b)). Its artifacts are *published rows* — each has
        // a key, a shape and, in every boundary set, a name and a parent — so supplied content,
        // computed content, `depends_on`, levels, any hierarchy and a layout pin are all things
        // its rows can carry. Computed content is cheap there because the flush resolves every
        // row's membership into a per-row source; the pin selects between the same forms it
        // selects between for an enumerated layer, and a label of its own is carried on its
        // record as an enumerated artifact's is. What stays refused on both is the proportional
        // criterion (above).
        let refuse = |what: &str| Err(DeclarationError::PredicateDeclares(what.to_string()));
        if matches!(self.membership, MembershipSource::Attribute(_)) {
            if !self.content.supplied.is_empty() {
                // A derived artifact has no publication to carry content bytes, and one served
                // without content its layer declares cannot be told from one whose content was
                // withheld (decision 0076) — the same refusal `resolve_or_mint` makes of a minted
                // key, made where the declaration is.
                return refuse("supplied content");
            }
            if !self.content.computed.is_empty() {
                // ⊘ A computed property is a function of `membership ∩ M_auth`, and reaching one
                // artifact's membership on an attribute level costs a scan of the whole column —
                // so it is refused here rather than served at a cost the declaration does not
                // show.
                return refuse("computed content");
            }
            if !self.depends_on.is_empty() {
                return refuse("depends_on");
            }
            if !self.levels.is_empty() {
                // The rule produces one artifact per value, at one resolution. A second level
                // would be a second rule nobody wrote.
                return refuse("levels");
            }
            if self.hierarchy.kind != HierarchyKind::Flat {
                return refuse("a hierarchy other than `flat`");
            }
            if self.layout.is_some() {
                return refuse("a layout pin");
            }
        }
        if matches!(self.membership, MembershipSource::Attribute(_))
            && self.artifact_visibility.carries_own_labels()
        {
            // A derived artifact carries no record of its own to read a label off.
            return refuse("`artifact_visibility.field`");
        }
        let label = |key: &str, word: &str| {
            crate::label::check_label(key, word).map_err(DeclarationError::Label)
        };
        if let Some(visibility) = &self.visibility {
            label("visibility", visibility)?;
        }
        if let MemberDefault::Label(default) = &self.artifact_visibility.default {
            label("artifact_visibility.default", default)?;
        }
        if let Some(field) = &self.artifact_visibility.field {
            if field.trim().is_empty() {
                return Err(DeclarationError::Label(
                    "`artifact_visibility.field` is empty. Name the column each artifact's own \
                     access label is read from, or omit the field"
                        .to_string(),
                ));
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

    /// What a **list** of keys naming this layer's artifacts means, position by position
    /// ([`ListMeaning`]).
    pub fn list_meaning(&self) -> ListMeaning {
        ListMeaning::of(self.hierarchy.kind, self.levels.len())
    }
}

// ---------------------------------------------------------------------------------------------
// A membership column's reading rule — shared by the build and the wire
// ---------------------------------------------------------------------------------------------

/// What the positions in a list of member keys mean (`artifacts-from-points.md` §4).
///
/// **The rule lives here because two implementations read it.** A build reads a member source's
/// key column out of Parquet; `/control/ingest` reads a column named for a layer out of an Arrow
/// batch. The *decode* cannot be shared — this crate carries no `arrow` dependency and is not
/// getting one — but the meaning must be, or the two entry points come to disagree about what a
/// caller's data says, which is exactly what
/// [decision 0091](../../../docs/decisions/0091-build-is-ingest-into-an-empty-database.md) forbids.
/// So the transport stays with each reader and the rule — which positions carry which level, which
/// adjacencies are edges, what a fixed arity must equal — is this type.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ListMeaning {
    /// `stacked` and `tiered`: entry *k* is the artifact at level *k*, one entry per declared
    /// level. `edges` is `tiered`'s containment between consecutive entries; `stacked`'s levels are
    /// independent analyses and carry none.
    Levelled { levels: usize, edges: bool },
    /// `nested`: a lineage, entry *k* the parent of entry *k+1*, **every artifact at level 0** —
    /// a treed layer's hierarchy is its edges and it declares no levels (decision 0082). A second
    /// lineage naming another parent for a child is refused (`artifacts-from-points.md` §4).
    Lineage,
    /// `flat` and `dag`: a membership each, at level 0, in no order. A flat layer has no positions
    /// for a list to index, so the entries are a set and nothing is read from their adjacency. A
    /// `dag` layer's list is the same set: a DAG node's ancestor closure has no linear order, so
    /// the list cannot be a lineage, and its edges are spelled on the artifact row's `parent` list
    /// only (`dag-hierarchies.md` §4, decision 0125).
    Unordered,
}

impl ListMeaning {
    pub fn of(kind: HierarchyKind, levels: usize) -> Self {
        match kind {
            HierarchyKind::Flat | HierarchyKind::Dag => ListMeaning::Unordered,
            HierarchyKind::Nested => ListMeaning::Lineage,
            HierarchyKind::Stacked => ListMeaning::Levelled {
                levels,
                edges: false,
            },
            HierarchyKind::Tiered => ListMeaning::Levelled {
                levels,
                edges: true,
            },
        }
    }

    /// The level the artifact at `position` belongs to.
    pub fn level_of(&self, position: usize) -> u32 {
        match self {
            ListMeaning::Levelled { .. } => position as u32,
            ListMeaning::Lineage | ListMeaning::Unordered => 0,
        }
    }

    /// Whether consecutive entries declare a parent edge.
    pub fn declares_edges(&self) -> bool {
        match self {
            ListMeaning::Levelled { edges, .. } => *edges,
            ListMeaning::Lineage => true,
            ListMeaning::Unordered => false,
        }
    }

    /// The length every row's list must have, where the declaration fixes one. `None` for a lineage
    /// and for plain multi-membership, whose rows are as long as each point's own branch.
    pub fn arity(&self) -> Option<usize> {
        match self {
            ListMeaning::Levelled { levels, .. } => Some(*levels),
            ListMeaning::Lineage | ListMeaning::Unordered => None,
        }
    }
}

/// **This point is in no artifact** — the sentinel every clusterer emits for noise
/// (`artifacts-from-points.md` §2).
///
/// Exactly `-1`, and not any negative: a negative id is otherwise unusual enough that swallowing
/// `-7` would more likely be eating data than handling noise.
pub const NOISE_KEY: i128 = -1;

/// The key an integer cell names, or `None` where it names no artifact.
///
/// **The decimal spelling is the key**, so `3` and `"3"` name one artifact whichever column type a
/// producer wrote — which is what lets a member table and an ingest batch spell one membership two
/// ways.
pub fn integer_key(value: i128) -> Option<String> {
    (value != NOISE_KEY).then(|| value.to_string())
}

/// The parent edges one row's entries declare: entry *k* is the parent of entry *k+1*.
///
/// **Adjacent entries only, and both of them present.** An entry naming no artifact is a point that
/// is noise at that resolution, not a link across it — reading past it would invent an edge from a
/// level to one two below, which is a containment claim the caller never made and which the next
/// point, clustered at that resolution, would contradict.
///
/// Generic in the entry, because the two readers hold different things at this point: the build
/// holds an interned address and the wire holds a key. The adjacency is the same rule either way,
/// and it is the half most likely to drift if each wrote its own.
pub fn parent_edges<T>(entries: &[Option<T>]) -> impl Iterator<Item = (&T, &T)> {
    entries
        .windows(2)
        .filter_map(|pair| match (&pair[0], &pair[1]) {
            (Some(parent), Some(child)) => Some((parent, child)),
            _ => None,
        })
}

/// The views a layer is drawn on, from the names its `views` gives: a group's name is every view
/// of the group, and any other name is one view. `group` answers a group's views, `None` where the
/// name is no group, and `view` whether a name is a view. `Err` is a name that is neither.
pub fn expand_views(
    declared: &[String],
    group: impl Fn(&str) -> Option<Vec<String>>,
    view: impl Fn(&str) -> bool,
) -> Result<Vec<String>, String> {
    let mut expanded = Vec::with_capacity(declared.len());
    for name in declared {
        match group(name) {
            Some(views) => expanded.extend(views),
            None if view(name) => expanded.push(name.clone()),
            None => return Err(name.clone()),
        }
    }
    Ok(expanded)
}

/// A name a group-scoped layer's `views` gives outside the scope's key set, and the groups that
/// hold that key set, for a refusal to name.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct OutsideScope {
    pub view: String,
    pub sharing: Vec<String>,
}

/// Refuse a layer scoped to `group` whose `views` names anything outside `group`'s key set. Its
/// artifacts are a set per view of that key set, so a view holding no key of it would draw none.
/// A name is admitted where it is `group`, a group declaring `members` of `group`, or a view of
/// either. `groups` is every group's name with the group it declares `members` of, and
/// `view_group` answers the group a view belongs to, `None` for a plain view or any other name.
pub fn check_scoped_views<'a>(
    declared: &[String],
    group: &str,
    groups: impl IntoIterator<Item = (&'a str, Option<&'a str>)>,
    view_group: impl Fn(&str) -> Option<String>,
) -> Result<(), OutsideScope> {
    let sharing: Vec<&str> = groups
        .into_iter()
        .filter(|(name, members_of)| *name == group || *members_of == Some(group))
        .map(|(name, _)| name)
        .collect();
    let admitted = |name: &str| {
        sharing.contains(&name) || view_group(name).is_some_and(|g| sharing.contains(&g.as_str()))
    };
    match declared.iter().find(|name| !admitted(name)) {
        None => Ok(()),
        Some(view) => Err(OutsideScope {
            view: view.clone(),
            sharing: sharing.iter().map(|name| name.to_string()).collect(),
        }),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_group_is_every_view_of_it_and_a_name_that_is_neither_is_refused() {
        let group = |name: &str| (name == "years").then(|| vec!["years:1".into(), "years:2".into()]);
        let view = |name: &str| name == "papers";
        let declared = |names: &[&str]| names.iter().map(|n| n.to_string()).collect::<Vec<_>>();
        assert_eq!(
            expand_views(&declared(&["papers", "years"]), group, view),
            Ok(declared(&["papers", "years:1", "years:2"]))
        );
        assert_eq!(
            expand_views(&declared(&["papers", "nowhere"]), group, view),
            Err("nowhere".to_string())
        );
    }

    #[test]
    fn a_scoped_layer_names_only_its_key_sets_groups_and_their_views() {
        let groups = [("years", None), ("decades", Some("years")), ("regions", None)];
        let view_group = |name: &str| name.split_once(':').map(|(group, _)| group.to_string());
        let check = |names: &[&str]| {
            let declared: Vec<String> = names.iter().map(|n| n.to_string()).collect();
            check_scoped_views(&declared, "years", groups, view_group)
        };
        assert_eq!(check(&["years", "decades", "years:2010", "decades:2010"]), Ok(()));
        let sharing = vec!["years".to_string(), "decades".to_string()];
        assert_eq!(
            check(&["years", "papers"]),
            Err(OutsideScope {
                view: "papers".to_string(),
                sharing: sharing.clone()
            })
        );
        assert_eq!(
            check(&["regions:north"]),
            Err(OutsideScope {
                view: "regions:north".to_string(),
                sharing
            })
        );
    }

    fn decl(kind: HierarchyKind, levels: Vec<u32>) -> LayerDeclaration {
        LayerDeclaration {
            scope: Default::default(),
            name: "clusters/x".into(),
            title: Some("X".into()),
            views: vec!["default".into()],
            membership: MembershipSource::Enumerated,
            value_set: Default::default(),
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
            layout: None,
            shape: None,
        }
    }

    /// **The three words are three variants**, and a word outside them is not a layout.
    /// `all` is the viewport request's word for every reachable layer, so no layer may carry it
    /// — in any case, since a request's spelling is checked exactly and a layer named `All`
    /// would read as the same word to a person.
    /// **One drawn geometry per layer** (`polygon-membership.md` §7.1): the kind follows the
    /// declaration, and any two of the three are refused naming both.
    #[test]
    fn a_layer_draws_one_shape_of_a_declared_kind_and_two_are_refused() {
        let authored = |ty: &str| SuppliedContent {
            name: "outline".into(),
            ty: ty.into(),
            require_member_visibility: SuppliedRequirement::Inherited,
        };
        let mut d = decl(HierarchyKind::Flat, vec![]);
        assert_eq!(d.drawn_shape(), None);
        d.content.computed = vec!["centroid".into(), "hull".into()];
        assert_eq!(d.drawn_shape(), Some(DrawnShape::Derived));
        assert!(d.validate().is_ok());

        let mut predicate = decl(HierarchyKind::Flat, vec![]);
        predicate.membership = MembershipSource::Spatial;
        predicate.shape = Some(ShapeDeclaration {
            kind: ShapeKind::Polygon,
        });
        assert_eq!(predicate.drawn_shape(), Some(DrawnShape::Predicate));
        assert!(predicate.validate().is_ok());

        for ty in ["polygon", "circle", "ellipse"] {
            let mut a = decl(HierarchyKind::Flat, vec![]);
            a.content.supplied = vec![authored(ty)];
            assert_eq!(a.drawn_shape(), Some(DrawnShape::Authored), "{ty}");
            assert_eq!(a.authored_shape().map(|(slot, _)| slot), Some(0));
            assert!(a.validate().is_ok(), "{ty}");
        }
        let mut text = decl(HierarchyKind::Flat, vec![]);
        text.content.supplied = vec![authored("text")];
        assert_eq!(text.drawn_shape(), None);
        assert_eq!(text.authored_shape(), None);

        // Each pair of the three, refused naming both.
        let mut hull_and_authored = d.clone();
        hull_and_authored.content.supplied = vec![authored("circle")];
        assert!(matches!(
            hull_and_authored.validate(),
            Err(DeclarationError::TwoDrawnGeometries(what)) if what.contains("hull") && what.contains("circle")
        ));
        let mut predicate_and_hull = predicate.clone();
        predicate_and_hull.content.computed = vec!["hull".into()];
        assert!(matches!(
            predicate_and_hull.validate(),
            Err(DeclarationError::TwoDrawnGeometries(_))
        ));
        let mut predicate_and_authored = predicate.clone();
        predicate_and_authored.content.supplied = vec![authored("polygon")];
        assert!(matches!(
            predicate_and_authored.validate(),
            Err(DeclarationError::TwoDrawnGeometries(_))
        ));
        let mut two_authored = decl(HierarchyKind::Flat, vec![]);
        two_authored.content.supplied = vec![authored("polygon"), authored("ellipse")];
        assert!(matches!(
            two_authored.validate(),
            Err(DeclarationError::TwoDrawnGeometries(_))
        ));
    }

    /// The ask vocabulary names the drawing, not its derivation: `shape` selects the hull a
    /// derived layer declared, and `hull` is not an ask word.
    #[test]
    fn the_ask_vocabulary_says_shape_where_the_declaration_says_hull() {
        assert_eq!(
            ComputedProperty::parse_ask("shape"),
            Some(ComputedProperty::Hull)
        );
        assert_eq!(ComputedProperty::parse_ask("hull"), None);
        assert_eq!(
            ComputedProperty::parse_ask("centroid"),
            Some(ComputedProperty::Centroid)
        );
        assert_eq!(
            ComputedProperty::parse_ask("box"),
            Some(ComputedProperty::Box)
        );
        assert_eq!(ComputedProperty::parse("shape"), None);
        assert_eq!(
            ComputedProperty::ASK_VOCABULARY,
            ["centroid", "box", "shape"]
        );
    }

    #[test]
    fn the_reserved_layer_selection_is_refused_as_a_name() {
        for name in ["all", "All", " all "] {
            let mut d = decl(HierarchyKind::Flat, Vec::new());
            d.name = name.into();
            assert!(
                matches!(d.validate(), Err(DeclarationError::ReservedName(_))),
                "{name:?} must be refused"
            );
        }
        let mut d = decl(HierarchyKind::Flat, Vec::new());
        d.name = "all/of/them".into();
        assert!(d.validate().is_ok(), "only the bare word is reserved");
    }

    #[test]
    fn the_pin_vocabulary_round_trips_and_admits_nothing_else() {
        for layout in [
            ServingLayout::ArtifactMajor,
            ServingLayout::RowMajorLabel,
            ServingLayout::RowMajorList,
        ] {
            assert_eq!(ServingLayout::parse_pin(layout.pin_word()), Some(layout));
            assert!(ServingLayout::PIN_VOCABULARY.contains(&layout.pin_word()));
        }
        assert_eq!(ServingLayout::parse_pin("row-major"), None);
        assert_eq!(ServingLayout::parse_pin("artifact-major"), None);
        assert_eq!(ServingLayout::parse_pin(""), None);
        assert!(!ServingLayout::ArtifactMajor.is_row_major());
        assert!(ServingLayout::RowMajorLabel.is_row_major());
        assert!(ServingLayout::RowMajorList.is_row_major());
    }

    /// **An attribute layer's serving form follows from its membership, so a pin is refused**; a
    /// spatial layer's membership is a per-row source once the flush resolves it, so a pin selects
    /// between the same forms it selects between for an enumerated layer (ruling (b)).
    #[test]
    fn a_layout_pin_is_refused_on_an_attribute_layer_and_taken_on_a_spatial_one() {
        for pin in [
            ServingLayout::ArtifactMajor,
            ServingLayout::RowMajorLabel,
            ServingLayout::RowMajorList,
        ] {
            let mut shape = decl(HierarchyKind::Flat, vec![]);
            shape.membership = MembershipSource::Spatial;
            shape.shape = Some(ShapeDeclaration {
                kind: ShapeKind::Polygon,
            });
            shape.layout = Some(pin);
            assert!(shape.validate().is_ok(), "{pin:?} on a shape layer");
            assert_eq!(RegisteredLayer::initial_layouts(&shape), vec![pin]);

            let mut attribute = decl(HierarchyKind::Flat, vec![]);
            attribute.membership = MembershipSource::Attribute("severity".into());
            attribute.layout = Some(pin);
            assert_eq!(
                attribute.validate(),
                Err(DeclarationError::PredicateDeclares("a layout pin".into()))
            );
        }
        // Both are fine with no pin at all.
        for source in [
            MembershipSource::Spatial,
            MembershipSource::Attribute("severity".into()),
        ] {
            let mut d = decl(HierarchyKind::Flat, vec![]);
            d.membership = source;
            d.layout = None;
            assert!(d.validate().is_ok());
        }
    }

    /// **The form a predicate layer is registered in is the form it is served in**, recorded at
    /// registration rather than left at the default for every request to disagree with.
    #[test]
    fn a_predicate_layers_recorded_form_follows_its_membership() {
        let mut shape = decl(HierarchyKind::Flat, vec![]);
        shape.membership = MembershipSource::Spatial;
        shape.shape = Some(ShapeDeclaration {
            kind: ShapeKind::Bbox,
        });
        // A shape layer is picked, not forced: with no pin it starts artifact-major, the form
        // every derived structure exists for, and the build's pass re-evaluates it.
        assert_eq!(
            RegisteredLayer::initial_layouts(&shape),
            vec![ServingLayout::ArtifactMajor]
        );
        shape.shape = None;
        assert_eq!(
            RegisteredLayer::initial_layouts(&shape),
            vec![ServingLayout::ArtifactMajor]
        );
        let mut attribute = decl(HierarchyKind::Flat, vec![]);
        attribute.membership = MembershipSource::Attribute("severity".into());
        assert_eq!(
            RegisteredLayer::initial_layouts(&attribute),
            vec![ServingLayout::RowMajorLabel]
        );
    }

    /// **The four kinds round-trip through their words, and a shape beside a membership that reads
    /// none is a rule nothing evaluates.**
    #[test]
    fn a_shape_kind_is_one_of_four_and_needs_a_spatial_membership() {
        for word in ShapeKind::VOCABULARY {
            let kind = ShapeKind::parse(word).expect(word);
            assert_eq!(kind.as_str(), word);
        }
        assert_eq!(ShapeKind::parse("radius"), None);
        for source in [
            MembershipSource::Enumerated,
            MembershipSource::Attribute("severity".into()),
        ] {
            let mut d = decl(HierarchyKind::Flat, vec![]);
            d.membership = source;
            d.shape = Some(ShapeDeclaration {
                kind: ShapeKind::Circle,
            });
            assert_eq!(
                d.validate(),
                Err(DeclarationError::ShapeWithoutSpatialMembership)
            );
        }
    }

    /// **A spatial layer may declare what an attribute layer may not** (ruling (b)): its artifacts
    /// are published rows, so content, a dependency, levels, a hierarchy and a pin all have a row
    /// to sit on, and so does an access label of its own.
    #[test]
    fn a_spatial_layer_may_declare_content_levels_and_a_hierarchy() {
        let base = || {
            let mut d = decl(HierarchyKind::Flat, vec![]);
            d.membership = MembershipSource::Spatial;
            d.shape = Some(ShapeDeclaration {
                kind: ShapeKind::Polygon,
            });
            d
        };
        let mut d = base();
        d.content.supplied = vec![SuppliedContent {
            name: "name".into(),
            ty: "text".into(),
            require_member_visibility: SuppliedRequirement::Inherited,
        }];
        d.content.computed = vec!["centroid".into(), "box".into()];
        d.depends_on = vec!["clusters/y".into()];
        assert!(d.validate().is_ok(), "{:?}", d.validate());

        let mut d = base();
        d.hierarchy.kind = HierarchyKind::Nested;
        assert!(d.validate().is_ok());

        let mut d = base();
        d.hierarchy.kind = HierarchyKind::Stacked;
        d.levels = vec![LevelDeclaration {
            level: 0,
            title: None,
            zoom: None,
        }];
        assert!(d.validate().is_ok());

        let mut d = base();
        d.artifact_visibility = ArtifactVisibility::carried("visibility");
        assert!(d.validate().is_ok());
    }

    /// A layer's labels are checked where the declaration is, so the build and a running service
    /// refuse the same words: an empty label, `inherited` where a label is expected, and an empty
    /// field.
    #[test]
    fn a_layers_labels_are_refused_where_they_cannot_be_read() {
        let mut d = decl(HierarchyKind::Flat, vec![]);
        d.visibility = Some(" ".into());
        assert!(matches!(d.validate(), Err(DeclarationError::Label(_))));

        let mut d = decl(HierarchyKind::Flat, vec![]);
        d.visibility = Some("inherited".into());
        assert!(matches!(d.validate(), Err(DeclarationError::Label(_))));

        let mut d = decl(HierarchyKind::Flat, vec![]);
        d.artifact_visibility.default = MemberDefault::Label("inherited".into());
        assert!(matches!(d.validate(), Err(DeclarationError::Label(_))));

        let mut d = decl(HierarchyKind::Flat, vec![]);
        d.artifact_visibility.field = Some("".into());
        assert!(matches!(d.validate(), Err(DeclarationError::Label(_))));

        let mut d = decl(HierarchyKind::Flat, vec![]);
        d.visibility = Some("team".into());
        d.artifact_visibility = ArtifactVisibility {
            field: Some("team".into()),
            default: MemberDefault::Label("public".into()),
        };
        assert!(d.validate().is_ok());
    }

    /// **What an attribute layer may not declare.** Each of these would register a layer that is
    /// reachable and serves nothing — the state the build already refuses for a layer declared in a
    /// view it does not write, and which no client can tell from one whose artifacts were all
    /// withheld.
    #[test]
    fn a_predicate_layer_declaring_what_it_cannot_carry_is_refused() {
        {
            let source = MembershipSource::Attribute("severity".into());
            let base = || {
                let mut d = decl(HierarchyKind::Flat, vec![]);
                d.membership = source.clone();
                d
            };
            assert!(base().validate().is_ok(), "the bare declaration is fine");

            let mut d = base();
            d.content.supplied = vec![SuppliedContent {
                name: "label".into(),
                ty: "text".into(),
                require_member_visibility: SuppliedRequirement::Inherited,
            }];
            assert!(matches!(
                d.validate(),
                Err(DeclarationError::PredicateDeclares(_))
            ));

            let mut d = base();
            d.content.computed = vec!["centroid".into()];
            assert!(matches!(
                d.validate(),
                Err(DeclarationError::PredicateDeclares(_))
            ));

            let mut d = base();
            d.depends_on = vec!["clusters/y".into()];
            assert!(matches!(
                d.validate(),
                Err(DeclarationError::PredicateDeclares(_))
            ));

            let mut d = base();
            d.hierarchy.kind = HierarchyKind::Stacked;
            d.levels = vec![LevelDeclaration {
                level: 0,
                title: None,
                zoom: None,
            }];
            assert!(matches!(
                d.validate(),
                Err(DeclarationError::PredicateDeclares(_))
            ));

            let mut d = base();
            d.artifact_visibility = ArtifactVisibility::carried("visibility");
            assert!(matches!(
                d.validate(),
                Err(DeclarationError::PredicateDeclares(_))
            ));
        }
    }

    /// A layer with no pin records artifact-major for every level it declares — one entry per
    /// level, and one for a layer that declares none.
    #[test]
    fn the_initial_record_is_one_entry_per_level() {
        let flat = decl(HierarchyKind::Flat, vec![]);
        assert_eq!(
            RegisteredLayer::initial_layouts(&flat),
            vec![ServingLayout::ArtifactMajor]
        );
        let mut stacked = decl(HierarchyKind::Stacked, vec![0, 1, 2]);
        stacked.layout = Some(ServingLayout::RowMajorList);
        assert_eq!(
            RegisteredLayer::initial_layouts(&stacked),
            vec![ServingLayout::RowMajorList; 3],
            "a pin is per layer and reaches every level of it"
        );
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
        // A DAG is `nested` in this respect (`dag-hierarchies.md` §3): its lineage is its edges.
        assert_eq!(
            decl(HierarchyKind::Dag, vec![0, 1]).validate(),
            Err(DeclarationError::TreeWithLevels)
        );
        assert!(decl(HierarchyKind::Dag, vec![]).validate().is_ok());
        assert_eq!(
            decl(HierarchyKind::Stacked, vec![]).validate(),
            Err(DeclarationError::StackedWithoutLevels)
        );
        assert!(decl(HierarchyKind::Stacked, vec![0, 1, 2])
            .validate()
            .is_ok());
    }

    #[test]
    fn a_tiered_layer_declares_edges_and_levels_together() {
        // The case the two-name shorthand does not cover: a ward is a ward everywhere, so the
        // resolution is semantic *and* the containment lineage exists. Validation must not force a
        // caller to throw one of them away.
        let mut d = decl(HierarchyKind::Stacked, vec![0, 1, 2]);
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
        for source in [
            MembershipSource::Spatial,
            MembershipSource::Attribute("severity".into()),
        ] {
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

        for ordinal in [
            0,
            1,
            RESERVED_BLOCK - 1,
            RESERVED_BLOCK,
            3 * RESERVED_BLOCK - 1,
        ] {
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
        assert!(
            object.contains_key("layout"),
            "the layout pin is an Option like any other here — `#[serde(default)]` is fine and \
             `skip_serializing_if` is not"
        );
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

    /// **§4's table, as the two readers read it.** A build reads a member table's key column and
    /// `/control/ingest` reads a column named for the layer; both ask this one type what a
    /// position means, so the table is asserted here rather than twice over Arrow.
    #[test]
    fn a_list_means_what_the_declared_hierarchy_says_it_means() {
        let levelled = ListMeaning::of(HierarchyKind::Tiered, 3);
        assert_eq!(levelled.arity(), Some(3), "one entry per declared level");
        assert_eq!(
            levelled.level_of(2),
            2,
            "entry k is the artifact at level k"
        );
        assert!(
            levelled.declares_edges(),
            "tiered entries contain one another"
        );

        let stacked = ListMeaning::of(HierarchyKind::Stacked, 3);
        assert_eq!(stacked.arity(), Some(3));
        assert!(
            !stacked.declares_edges(),
            "stacked levels are independent analyses, so adjacency states nothing"
        );

        let lineage = ListMeaning::of(HierarchyKind::Nested, 0);
        assert_eq!(
            lineage.arity(),
            None,
            "a lineage is as deep as its own branch"
        );
        assert_eq!(
            lineage.level_of(2),
            0,
            "a nested layer holds every artifact at level 0"
        );
        assert!(lineage.declares_edges());

        let flat = ListMeaning::of(HierarchyKind::Flat, 0);
        assert_eq!(flat.arity(), None);
        assert_eq!(flat.level_of(7), 0);
        assert!(
            !flat.declares_edges(),
            "a flat list is plain multi-membership — a set, with no positions to read"
        );
        assert_eq!(
            ListMeaning::of(HierarchyKind::Dag, 0),
            flat,
            "a dag layer reads a list as flat does: a DAG node's closure is a set, not a chain, \
             so the list is memberships and its edges come from the artifact row's parent list \
             alone (decision 0125)"
        );
    }

    /// **The gap is not an edge.** An entry naming no artifact is a point that is noise at that
    /// resolution, and reading past it would state a containment no row makes.
    #[test]
    fn an_entry_naming_nothing_links_nothing_across_itself() {
        let full = [Some("a"), Some("b"), Some("c")];
        assert_eq!(
            parent_edges(&full).collect::<Vec<_>>(),
            vec![(&"a", &"b"), (&"b", &"c")]
        );

        let gapped = [Some("a"), None, Some("c")];
        assert!(
            parent_edges(&gapped).next().is_none(),
            "a point clustered at level 0 and level 2 and noise between declares no edge at all"
        );
    }

    /// `-1` and nothing else: a negative id is otherwise unusual enough that swallowing `-7` would
    /// more likely be eating data than handling noise.
    #[test]
    fn only_minus_one_means_this_point_is_in_no_artifact() {
        assert_eq!(integer_key(-1), None);
        assert_eq!(integer_key(-7), Some("-7".to_string()));
        assert_eq!(integer_key(3), Some("3".to_string()));
    }
}
