//! The configuration file: one TOML document declaring the corpus, its views, its vocabularies,
//! its attributes and its layers.
//!
//! [`configuration.md`](../../../docs/design/configuration.md) is the design and its §1 is the
//! contract — the whole surface in one table. **The set is closed**, and that is a property rather
//! than an accident: every block parses under `deny_unknown_fields`, and every value that is a word
//! rather than a caller's string is drawn from an enumerated set. Closure is what the leak register
//! rests on, the register being exhaustive *because* the surface is enumerable, so a key this
//! module accepts without an entry in §1 is a disclosure control nobody has reasoned about.
//! [`the_accepted_key_set_is_configuration_ms_table`] is the assertion that keeps the two in step.
//!
//! ## Two axes, and only two
//!
//! Everything about who may see an object answers one of two questions
//! ([decision 0088](../../../docs/decisions/0088-visibility-is-two-axes-and-the-membership-test-is-one.md)):
//! `visibility` asks which access label the viewer must hold, and `require_member_visibility` asks
//! how much of the object's own membership the viewer must already see. That retires `listing`,
//! `gate`/`ungated`, `artifacts_carry_own`, `visible_when` and supplied content's `corpus_derived`
//! — three spellings of the first question and three settings of the second. Every one of them is
//! now refused by the unknown-field rule rather than aliased: decision 0048's shape, replaced
//! rather than carried, which is what tells a caller their file is stale instead of reading it
//! wrong.
//!
//! **The value space is closed per site, and a bare word outside it is never read as a label.** A
//! layer's `visibility` takes an access label or `public`; a vocabulary's takes exactly `public` or
//! `derived`. So `visibility = "derrived"` on a vocabulary is a refusal, where under a
//! label-shaped reading it would be a value set gated on a label nobody holds — or, worse on the
//! other side of a typo, published.
//!
//! ## Why so much of this file is refusals
//!
//! The parse rules are the design. §6's rule — *performance knobs default; disclosure controls do
//! not* — means every required control is parsed as an `Option` and hand-validated, so the message
//! can teach: what is missing, the values spelled out, what each one does, and why there is no
//! default. Serde's own "missing field" text does none of that, and a control whose absence is
//! reported as a schema error rather than as a decision is one an author will fill in with the
//! first value that makes the error go away. The refusals that are not merely hygiene:
//!
//! - **An attribute naming a vocabulary no `[[vocabulary]]` block declares**, refused here rather
//!   than when a data file is opened — and never an implicitly minted open vocabulary. A typo in
//!   the reference would otherwise create a value set nobody authored, at whatever visibility the
//!   fall-through picked.
//! - **Code `0` in a value set** is the *absent* sentinel (`per-point-attributes.md` §3.6).
//!   Accepting `low = 0` would make every value-less row a member of `low` — a wrong membership
//!   set, silently.
//! - **A key in both `reserved` and the live set.** `reserved` is Protobuf's mechanism and carries
//!   its reasoning: a retired code is never reassigned, because reusing one silently recolours
//!   history.
//! - **An access label spelled `inherited`.** It is the one reserved word occupying a slot that
//!   otherwise takes a caller's label, so a layer gated on a real term called `inherited` and one
//!   declaring *the container's gate is the whole of it* would be the same eight characters.
//!
//! ## Codes are pinned or assigned, and both are recorded
//!
//! A value set is *inline or sourced*, and its codes are *pinned or assigned* — two independent
//! choices. `values = ["low", "high"]` assigns in the order given; `[vocabulary.values]` with
//! `low = 1` pins. **A caller who does not care which integer a value gets should not have to
//! invent one**: pinning exists so a rebuild preserves codes, not because choosing them is part of
//! declaring a vocabulary. Assigned codes are recorded in the compiled vocabulary exactly as
//! pinned ones are, so `MANIFEST.json` is the record either way.
//!
//! ⊘ **The carry rule is not built.** `configuration.md` §1 states that a rebuild replays the
//! recorded codes — a value keeps its code, a new value takes the next free one, a removed value's
//! code moves to `reserved` — so that reordering a list cannot recolour stored rows. Nothing reads
//! a previous build's manifest yet, so **reordering a bare key list today reassigns its codes**.
//! Pin the codes to hold them still.
//!
//! ## Acquisition: sources, defaults, fields and the override
//!
//! **`[sources]` names every file this declaration reads, one path each, relative to this
//! document** (`configuration.md` §3, §8). A relative path travels in git with the file describing
//! it and is exactly as reproducible as the declaration around it, which an invocation naming five
//! files was not; an **absolute or machine-specific** path is refused there, and `--file NAME=PATH`
//! is where one goes instead. Every `source` elsewhere in the document **names one of those keys**,
//! so a file three blocks read is one path rather than three to keep in step. There is no
//! name-or-path fallback: a name `[sources]` does not carry is refused listing the names that do
//! exist, because reading it as a relative path would turn a typo into a missing file rather than
//! a declaration that does not resolve.
//!
//! **`[defaults]` is what `[corpus]` was, with the constraint removed** ([`Defaults`]). It carries
//! a `source` and an `entity_id_field`, and any block that reads either may write its own — so an
//! attribute may name its own file and its own identity column, because a file that carries entity
//! ids can be joined whatever it calls them. What `[corpus]` guaranteed, that every attribute lands
//! in one entity space, is guaranteed by the entity id and never was by the file.
//!
//! **`--file` is an override, never a binding.** Its key is the *source's own name*, so one
//! override moves every block reading that file at once — where the previous object-keyed form
//! (`corpus`, `view:s0`, `view:s0:point_visibility`, …) needed one per block and left the one you
//! missed quietly reading the old file. Three rules, all fail-closed and unchanged in substance:
//!
//! - a `source` naming **no `[sources]` key** is a refusal listing the names that exist;
//! - an override naming **no `[sources]` key** is a refusal too — otherwise a typo in the name
//!   leaves the config's own path quietly in force under a command line that says otherwise;
//! - an override **never creates** a source. It replaces a path `[sources]` already writes, so a
//!   closed vocabulary cannot be opened, and a view cannot acquire geometry, from the command line
//!   alone.
//!
//! ## The extent is a property of the view
//!
//! A coordinate is quantised across the view's extent into 32 bits, and quantisation **clamps** —
//! so two bundles built from one corpus under different extents place the same point in different
//! cells and both are well-formed. That is why it is declared here and not passed at invocation.
//! [`Extent::Auto`] is the one value this module cannot resolve on its own: [`resolve_extent`]
//! reads the view's points source to fit the box, which is legitimate precisely because the
//! alternative is an operator guessing a frame their data has already decided.
//!
//! **A `fields` map says *where*, never *whether*.** The object's own keys assert that a field
//! exists — `hierarchy` that there are parent edges, `depends_on` that there are attachment edges,
//! `content.supplied` that there is content — and the map only locates what is already declared. So
//! a name the object never declared is refused, and so is a name that is not one of that object's
//! fields at all: both would otherwise read as *this file has one*, which is a claim the map is not
//! allowed to make.
//!
//! **A field map moves a field, and the reader takes the name it moved it to.** [`Fields`] is the
//! resolved answer — the declared name where the map moved one, the canonical name everywhere else
//! — and it is carried to the reader rather than consulted here. Two refusals split across the two
//! places that can make them: this module refuses a name the object does not have and a name the
//! object never declared, because both are answerable from the declaration alone; the *readers*
//! refuse a name the file does not carry, because that needs the file open.
//!
//! **A layer's map reaches its readers too.** One source per layer is what made that possible:
//! there is no `layer` discriminator column left to select on, an artifact is one row carrying its
//! `contents` as a ranked list, and both readers take the names the map resolved. The two fields a
//! map may not move are `level` and `attached_level`, which `configuration.md` §1's table does not
//! name — they are read under their own names or not at all.
//!
//! **Or the artifacts are written out here**, `artifacts = [{ key = …, contents = [ … ] }]`, for a
//! layer a person authors rather than a pipeline produces. It is a spelling and not a second kind
//! of layer: the planned artifacts are the same, so the bundle is byte-identical either way.
//!
//! ## The access relation, in three shapes
//!
//! A view says where each point's access terms are and what a point carrying none gets
//! ([`AccessInput`]): a `list<string>` field of its own source, a separate exploded
//! `(entity_id, term_id)` relation, or neither — every point taking the default, which is the
//! corpus with no permission model. `default` is required on all three, because a point's label has
//! to come from somewhere and *nowhere* is a decision rather than an omission.
//!
//! **Filling never overrides**, and that is inadmissible rather than merely unwise: a point's terms
//! are disjunctive — `M_auth` is a union of posting lists — so a label added to a point can only
//! widen it. A null value and an empty list both mean *no access terms*, which means visible to no
//! principal; neither means unrestricted, and where a default is declared those are the rows it
//! fills.

use std::collections::{BTreeMap, BTreeSet, HashMap, HashSet};
use std::path::{Path, PathBuf};

use serde::Deserialize;
use tessera_spatial::tiler::ScalarType;
use tessera_spatial::{cell, Bounds};
use tessera_store::vocabulary::VocabularyMinter;
use tessera_types::layer::{
    ArtifactVisibility, ContentDeclaration, ExistenceCriterion, Hierarchy, HierarchyKind,
    LayerDeclaration, LevelDeclaration, MemberDefault, MembershipSource, ServingLayout,
    ShapeDeclaration, ShapeKind, SuppliedContent, SuppliedRequirement,
};

use crate::error::{BuildError, Result};
use crate::input::{CoordinateSurvey, PointSurvey};

/// A parse or consistency failure in the config, or in a vocabulary file bound to it.
///
/// One variant carrying a message rather than a variant per rule: every one of these is a build
/// refusal an operator reads and fixes, none is caught and branched on, and a rule added as a
/// message cannot go uncaught in a `match` somewhere else.
pub fn declaration_error(detail: impl Into<String>) -> BuildError {
    BuildError::Declaration(detail.into())
}

/// The reserved *absent* code (`per-point-attributes.md` §3.6). Excluded from every value block and
/// from minting, so a row carrying no value for a column is distinguishable from one carrying the
/// first value.
pub const ABSENT_CODE: u32 = 0;

/// The word that reaches every principal, wherever an access label may be written.
const PUBLIC: &str = "public";
/// The word that means *the container's gate is the whole of it*. It occupies a slot that otherwise
/// takes a caller's label, so a label spelled this way is refused (§4).
const INHERITED: &str = "inherited";
/// **The canonical identity field** (`configuration.md` §8), which
/// `[defaults].entity_id_field` moves for this declaration and each reader of one may move again.
/// It is entity-space and shared: a point has one identity across every view it appears in, and it
/// is what a member row names. Not `entity`, which names the object rather than the value, and not
/// `id`, which collides with `tessera_id` and with an external id.
pub const ENTITY_ID: &str = "entity_id";

// ---------------------------------------------------------------------------------------------
// The file, as written
// ---------------------------------------------------------------------------------------------

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct ConfigFile {
    /// `[sources]` — the caller's own names for the files this declaration reads.
    #[serde(default)]
    sources: Option<BTreeMap<String, String>>,
    /// `[defaults]` — what a block takes when it names neither of these itself.
    #[serde(default)]
    defaults: Option<DefaultsBlock>,
    #[serde(default)]
    view: Vec<ViewBlock>,
    #[serde(default)]
    vocabulary: Vec<VocabularyBlock>,
    #[serde(default)]
    attribute: Vec<AttributeBlock>,
    #[serde(default)]
    layer: Vec<LayerBlock>,
}

/// `[defaults]` — the source and the identity column a block takes when it names neither.
///
/// **This is what `[corpus]` was, minus the constraint that made it a block.** `[corpus]` named
/// the one file every attribute was read from and the one column its identity sat in, and nothing
/// could say otherwise; here both are defaults and any block that reads a source or an entity id
/// may write its own. What `[corpus]` guaranteed — that every attribute lands in one entity space
/// — is guaranteed by the entity id and never was by the file.
#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct DefaultsBlock {
    /// A name in `[sources]`, taken by a `[[view]]` or an `[[attribute]]` that names none.
    #[serde(default)]
    source: Option<String>,
    /// The column an entity id is read from, wherever one is read under the canonical name.
    #[serde(default)]
    entity_id_field: Option<String>,
}

/// `[[view]]` — one named coordinate system.
#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct ViewBlock {
    name: String,
    #[serde(default)]
    title: Option<String>,
    #[serde(default)]
    source: Option<String>,
    #[serde(default)]
    fields: Option<BTreeMap<String, String>>,
    /// The quantisation frame, in any of §1's four spellings. Held as a `toml::Value` and
    /// resolved by [`compile_extent`] rather than typed here, because an untagged enum over a
    /// string and a table reports every mistake inside the table as *matched no variant* — and
    /// this key's mistakes (a `margin` beside a `min`, an `x` without a `y`) are exactly the ones
    /// worth naming. The table form is still a `deny_unknown_fields` struct, so its key set is
    /// closed and readable out of serde's own message.
    #[serde(default)]
    extent: Option<toml::Value>,
    #[serde(default)]
    point_visibility: Option<PointVisibilityBlock>,
    #[serde(default)]
    visibility: Option<String>,
}

/// `{ field, default }` or `{ source, default }` — where each point's own label is, and what one
/// carrying none gets.
///
/// **A point's label comes from a field or from a source, never both** (`configuration.md` §1).
/// `field` names a column of the view's own source; `source` names a separate exploded
/// `(entity_id, term_id)` relation, which is the shape the probe generators produce natively at
/// 10⁹ and the one the build writes as oracle output regardless. `default` alone is legal and is
/// the corpus with no permission model.
#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct PointVisibilityBlock {
    #[serde(default)]
    field: Option<String>,
    #[serde(default)]
    source: Option<String>,
    #[serde(default)]
    default: Option<String>,
}

/// `extent`'s table form, in one struct with every key optional and the combinations checked by
/// hand ([`compile_extent`]).
///
/// One struct rather than three, because the three shapes overlap in exactly the ways a caller
/// gets wrong — `margin` beside `min`, an `x` without a `y`, `auto` beside a stated box — and
/// three variants would report each of those as *no variant matched*. The key set stays closed
/// under `deny_unknown_fields`, which is what `configuration.md` §1's table is asserted against.
#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct ExtentTable {
    #[serde(default)]
    auto: Option<bool>,
    #[serde(default)]
    margin: Option<f64>,
    #[serde(default)]
    min: Option<f64>,
    #[serde(default)]
    max: Option<f64>,
    #[serde(default)]
    x: Option<[f64; 2]>,
    #[serde(default)]
    y: Option<[f64; 2]>,
}

/// `{ field, default }` — where each artifact's own label is, and what one carrying none gets.
///
/// **The presence of `field` is the declaration that artifacts carry their own labels** (C27),
/// which is why it is one table rather than a flag beside a fallback: the two cannot be declared
/// apart. It takes no `source`: an artifact's label rides its own row, there being one row per
/// artifact, where a point's label is one of many terms and needs a relation of its own.
#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
struct ArtifactVisibilityBlock {
    #[serde(default)]
    field: Option<String>,
    #[serde(default)]
    default: Option<String>,
}

/// `[[vocabulary]]` — a named value set.
#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct VocabularyBlock {
    name: String,
    #[serde(default)]
    title: Option<String>,
    #[serde(default)]
    width: Option<String>,
    #[serde(default)]
    value_set: Option<String>,
    #[serde(default)]
    visibility: Option<String>,
    #[serde(default)]
    source: Option<String>,
    #[serde(default)]
    fields: Option<BTreeMap<String, String>>,
    /// Inline values: an array of keys, or a `key = code` table. One type for both, because which
    /// one was written decides only whether the codes are pinned.
    #[serde(default)]
    values: Option<toml::Value>,
    #[serde(default)]
    reserved: Option<Vec<i64>>,
}

/// `[[attribute]]` — one per-point column, read from the source it names or from
/// `[defaults].source`.
///
/// `deny_unknown_fields` throughout: a mistyped key in a disclosure control is the one class of
/// typo that must not read as a default. `vocabluary = "severity"` under a serde that ignores
/// unknown fields is a category with no value set, declared by someone who believed they had said
/// otherwise.
#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct AttributeBlock {
    name: String,
    #[serde(default)]
    title: Option<String>,
    #[serde(default)]
    field: Option<String>,
    /// The `[sources]` name this column is read from. Absent takes `[defaults].source`, and a
    /// declaration with neither is refused: a column has to be read from somewhere.
    #[serde(default)]
    source: Option<String>,
    /// The column this source spells the entity id in. Absent takes `[defaults].entity_id_field`,
    /// which defaults to `entity_id` — a file carrying entity ids joins whatever it calls them.
    #[serde(default)]
    entity_id_field: Option<String>,
    #[serde(rename = "type", default)]
    ty: Option<String>,
    #[serde(default)]
    vocabulary: Option<String>,
    /// The two placement booleans (records §2), each defaulting `false` — the cheapest placement,
    /// made more expensive only by an explicit word.
    #[serde(default)]
    render: bool,
    #[serde(default)]
    index: bool,
    #[serde(default)]
    multi: bool,
    #[serde(default)]
    render_in: Option<Vec<String>>,
    /// Which analyser a `text` column's terms are produced by, by name (decision 0070). Absent
    /// means [`tessera_analyse::UNICODE`]; present on a non-`text` column is refused, because an
    /// analyser a column does not use is a setting its author believes is in effect.
    #[serde(default)]
    analyser: Option<String>,
}

/// `[[layer]]` — one annotation layer. Artifact-side semantics are `annotation-write-cycle.md`
/// §6.1's; this is the declaration.
#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
struct LayerBlock {
    name: String,
    #[serde(default)]
    title: Option<String>,
    /// `[layer.members]` — membership as its own source, one row per `(artifact, entity)`, for a
    /// membership no single cell should hold.
    #[serde(default)]
    members: Option<MembersBlock>,
    /// `[layer.labels]` — sugar, expanded to a layer of its own before anything here compiles
    /// ([`expand_labels`]).
    #[serde(default)]
    labels: Option<LabelsBlock>,
    #[serde(default)]
    views: Option<Vec<String>>,
    #[serde(default)]
    source: Option<String>,
    #[serde(default)]
    fields: Option<BTreeMap<String, String>>,
    /// The artifacts written out in the document itself, instead of `source`
    /// (`configuration.md` §1). Typed here rather than held as a `toml::Value`, so the inline
    /// row's own key set is closed by the same `deny_unknown_fields` rule every block is under.
    #[serde(default)]
    artifacts: Option<Vec<InlineArtifact>>,
    /// `"enumerated"`, `"spatial"` or `{ attribute = "<field>" }`. Held as a `toml::Value`
    /// because the third spelling is a table naming the field the predicate reads, and a
    /// hand-written match reports the three shapes as three shapes rather than as *no variant
    /// matched* ([`compile_membership`]).
    #[serde(default)]
    membership: Option<toml::Value>,
    /// `"closed"` (the default) or `"open"` — whether a member key no artifact declares is refused
    /// or creates one (`artifacts-from-points.md` §3).
    #[serde(default)]
    value_set: Option<String>,
    #[serde(default)]
    hierarchy: Option<HierarchyBlock>,
    /// `layout` — the serving-layout pin, `"rows"`, `"column"` or `"list"`. Absent, the pick is
    /// automatic and re-evaluated at every fold (decision 0094).
    #[serde(default)]
    layout: Option<String>,
    #[serde(default)]
    visibility: Option<String>,
    #[serde(default)]
    artifact_visibility: Option<ArtifactVisibilityBlock>,
    #[serde(default)]
    require_member_visibility: Option<toml::Value>,
    #[serde(default)]
    withdraw_on_member_deletion: Option<bool>,
    #[serde(default)]
    depends_on: Vec<String>,
    #[serde(default)]
    levels: Vec<LevelBlock>,
    #[serde(default)]
    content: Option<ContentBlock>,
    /// `[layer.shape]` — what a `membership = "spatial"` layer's artifacts are shaped like, and
    /// how deep the tiles that cover them are drawn ([`compile_shape`]).
    #[serde(default)]
    shape: Option<ShapeBlock>,
}

/// `[layer.shape]` as written. Both keys are optional *here* and neither is optional in the
/// compiled form: absence is what makes the message name the key rather than reporting *no variant
/// matched*, which is the same reason `membership` is held as a `toml::Value`.
#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
struct ShapeBlock {
    /// ⊘ `"bbox"`, and absent means `"bbox"` — one kind, so a spelling is a courtesy rather than a
    /// choice, and any other word is refused.
    #[serde(default)]
    kind: Option<String>,
    /// The Morton depth the shape is covered at. **Required**, because it is the membership.
    #[serde(default)]
    depth: Option<i64>,
}

/// One artifact written into the document itself — `artifacts = [{ key = …, contents = [ … ] }]`.
///
/// **For what a person authors**, a dozen curated regions rather than a corpus
/// (`annotation-write-cycle.md` §6.1). Its keys are the artifact grain's canonical field names and
/// nothing else: an inline row *is* the canonical spelling, so there is no `fields` map to move one
/// — which is why declaring both is refused.
#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct InlineArtifact {
    /// The caller's own name for the artifact, which is what an edge into it names.
    pub key: String,
    /// The resolution this artifact sits at. `0` for a layer with no levels.
    #[serde(default)]
    pub level: u32,
    /// The membership, by inclusion.
    #[serde(default)]
    pub members: Option<Vec<u64>>,
    /// The membership, by exclusion — the entities it leaves out. Complemented once at build
    /// against the view's entity set, so the published artifact is the one `members` would have
    /// produced (`annotation-write-cycle.md` §6.1). Declaring both is refused.
    #[serde(default)]
    pub excluding: Option<Vec<u64>>,
    /// The ranked contents, best first: one entry per rank, each a value per supplied kind.
    #[serde(default)]
    pub contents: Vec<Vec<String>>,
    /// The artifact's bounding box, `[min_x, min_y, max_x, max_y]` — the shape a
    /// `membership = "spatial"` layer's membership is drawn from, and refused on every other kind.
    #[serde(default)]
    pub bbox: Option<Vec<f64>>,
    /// The parent artifact in a hierarchy, by its key.
    #[serde(default)]
    pub parent: Option<String>,
    #[serde(default)]
    pub attached_layer: Option<String>,
    #[serde(default)]
    pub attached_level: u32,
    #[serde(default)]
    pub attached_key: Option<String>,
}

/// `[layer.labels]` — a label layer, written where it is used.
///
/// **Sugar, and sugar exactly**: it carries no key that is not a `[[layer]]` key, and it expands
/// to a `[[layer]]` block before anything compiles, so a declaration written this way and the
/// same one written out as a second layer build a byte-identical bundle
/// (`annotation-write-cycle.md` §6.1). What the expansion supplies is mechanical — the parent's
/// views, a flat hierarchy, `depends_on` the parent, and the content wrapper around `type`. What
/// it never supplies is the gate, the membership requirement or the existence of membership data,
/// each of which is written out here.
#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
struct LabelsBlock {
    name: String,
    #[serde(default)]
    title: Option<String>,
    #[serde(default)]
    source: Option<String>,
    #[serde(default)]
    fields: Option<BTreeMap<String, String>>,
    /// `[layer.labels.members]` — the same block a `[[layer]]` takes, and here for the same
    /// reason: a label's ranked contents each name the generating set they were drawn from, and a
    /// `(artifact, rank, entity)` row is the only shape that carries one. Without it the sugar
    /// could declare content it could never serve.
    #[serde(default)]
    members: Option<MembersBlock>,
    #[serde(rename = "type", default)]
    ty: Option<String>,
    /// Written out, never derived: a label's members **are** its generating set, and no build can
    /// work out from the parent which entities a synthesis was drawn from.
    #[serde(default)]
    membership: Option<toml::Value>,
    /// A **threshold**: how much of a label's membership a viewer must already see for the label
    /// to appear at all. `[layer.labels.content]`'s key is not the same dial at a second grain —
    /// it is a provenance declaration — which is why neither can carry the other.
    #[serde(default)]
    require_member_visibility: Option<toml::Value>,
    /// `[layer.labels.content]` — where the text came from, declared and never supplied. `all`
    /// says it is a synthesis of the members, so it is read only where every document behind it
    /// can be; `inherited` says it is true whether or not any of them exists — a name a person
    /// wrote — and adds no requirement beyond the artifact's gate. Only the caller knows which,
    /// and the expansion fixing it at `all` decided a disclosure control on their behalf.
    #[serde(default)]
    content: Option<LabelsContentBlock>,
    /// Declared, never supplied. It is a disclosure control, so it has no default: the value an
    /// expansion could pick for a caller who wrote nothing is a value the caller never chose.
    #[serde(default)]
    artifact_visibility: Option<ArtifactVisibilityBlock>,
    /// The one defaulted disclosure control in the surface: absent, this layer takes its parent's
    /// gate. Admissible only because the value it defaults to is the parent's own and never the
    /// widest one there is ([`expand_labels`]).
    #[serde(default)]
    visibility: Option<String>,
}

/// `[layer.labels.content]` — the label content's own member requirement. One key, because the
/// rest of `[layer.content]` has no meaning here: a label layer's content is the label, supplied
/// by the caller, so there is nothing to compute and the wrapper the expansion writes is the
/// caller's `type` at the caller's requirement.
#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
struct LabelsContentBlock {
    #[serde(default)]
    require_member_visibility: Option<String>,
}

/// `[layer.members]` — membership as its own source, instead of a list field on the artifact row.
/// Declaring both is refused (`configuration.md` §7).
#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
struct MembersBlock {
    #[serde(default)]
    source: Option<String>,
    #[serde(default)]
    fields: Option<BTreeMap<String, String>>,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
struct HierarchyBlock {
    #[serde(default)]
    kind: Option<String>,
    /// A rendering default and the one key here carrying no disclosure argument in either
    /// direction: every artifact served has passed its own test independently.
    #[serde(default)]
    prune_children: bool,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
struct LevelBlock {
    #[serde(default)]
    level: Option<u32>,
    #[serde(default)]
    title: Option<String>,
    #[serde(default)]
    zoom: Option<(u32, u32)>,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
struct ContentBlock {
    #[serde(default)]
    computed: Vec<String>,
    #[serde(default)]
    supplied: Vec<SuppliedBlock>,
    #[serde(default)]
    withdraw_on_member_deletion: Option<bool>,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
struct SuppliedBlock {
    name: String,
    #[serde(rename = "type", default)]
    ty: Option<String>,
    #[serde(default)]
    require_member_visibility: Option<String>,
}

// ---------------------------------------------------------------------------------------------
// The compiled form
// ---------------------------------------------------------------------------------------------

/// A parsed, checked configuration: entity space, the coordinate systems over it, and the layers
/// drawn on them.
#[derive(Debug, Clone, Default)]
pub struct Config {
    /// Entity space: the attributes and the vocabularies they draw on.
    pub schema: Schema,
    /// The declared attributes **grouped by the source they are read from**, in the order each
    /// group's first attribute was declared. One group is one pass over one file.
    pub attribute_sources: Vec<AttributeSource>,
    pub views: Vec<View>,
    /// In declaration order, which is registration order: a layer must be declared after every
    /// layer it names in `depends_on`.
    pub layers: Vec<LayerDeclaration>,
    /// Which of [`Config::layers`] the `[layer.labels]` sugar wrote, and which layer each was
    /// written for.
    ///
    /// **Recorded because nothing else can recover it.** The expansion is textual and happens
    /// before anything compiles, so a label layer and the same layer written out by hand are
    /// indistinguishable everywhere below it — which is exactly the property the sugar claims. A
    /// reviewer diffing two builds' disclosure decisions still needs to know that a layer they did
    /// not write appeared because a `[layer.labels]` block asked for it.
    pub label_layers: BTreeMap<String, String>,
    /// Each layer's bound sources, parallel to [`Config::layers`] and by the same name.
    ///
    /// **Beside the declarations rather than inside them.** A [`LayerDeclaration`] is exactly what
    /// `PUT /control/layers` takes (`configuration.md` §2), and a deployment writing through the
    /// service omits acquisition entirely — so a source living on the declaration would be a build
    /// input the control plane would have to carry and ignore.
    pub layer_sources: Vec<LayerSources>,
}

/// One file the attribute pass reads, and which declared columns it carries.
///
/// **The grouping is the join.** Every attribute naming one source is read in one merge sweep
/// over that file against this build's assigned ordinals, so a declaration whose columns sit in
/// three files pays three passes rather than one impossible one. Two attributes sharing a file but
/// joining on different identity columns are two groups, because the join key is half of what a
/// group is.
#[derive(Debug, Clone)]
pub struct AttributeSource {
    /// The caller's own name for the file, from `[sources]` — what a refusal and the coverage
    /// report quote, because it is the word the caller wrote.
    pub name: String,
    /// The file itself, resolved against the declaring document and after any `--file` override.
    pub path: PathBuf,
    /// Where this source's identity field sits. Canonical is `entity_id`; `[defaults]` and each
    /// attribute's own `entity_id_field` move it.
    pub fields: Fields,
    /// Which of [`Schema::attributes`] this file carries, by index, in declaration order. Indices
    /// rather than names because the scalar tail is stored positionally: the declaration's order
    /// is the column order, and a group is a subset of it rather than a reordering.
    pub attributes: Vec<usize>,
}

impl AttributeSource {
    /// One group over every declared attribute, read from one file under canonical names.
    ///
    /// For a caller building arguments programmatically — the benches, the fixtures and the
    /// correctness corpus — where the declaration would have been `[defaults].source` and nothing
    /// else. Empty for an empty schema, which is the whole of what such a build acquires.
    pub fn over(path: impl Into<PathBuf>, schema: &Schema) -> Vec<AttributeSource> {
        if schema.is_empty() {
            return Vec::new();
        }
        vec![AttributeSource {
            name: "corpus".to_string(),
            path: path.into(),
            fields: Fields::canonical("the attribute source"),
            attributes: (0..schema.attributes.len()).collect(),
        }]
    }
}

/// One layer's bound acquisition keys.
///
/// **One source per layer**, which is what removes the discriminator: there is no `layer` column
/// to select on, no filter to configure, and no way for a layer to ingest another's rows
/// (`annotation-write-cycle.md` §6.1).
#[derive(Debug, Clone)]
pub struct LayerSources {
    pub name: String,
    /// Where this layer's artifacts come from — its own file, or the rows written inline. `None`
    /// for a layer declared and empty, which is legal (`configuration.md` §2).
    pub artifacts: Option<ArtifactSource>,
    /// `[layer.members].source` — one row per `(artifact, entity)`.
    pub members: Option<MemberSource>,
}

/// One layer's artifacts: the file it names, or the rows the document carries itself.
///
/// **Two spellings of one thing.** Both produce the same planned artifacts, so a layer written
/// inline and the same layer written to a file build a byte-identical bundle — the inline route
/// exists for what a person authors, not for a different kind of artifact.
#[derive(Debug, Clone)]
pub enum ArtifactSource {
    /// `[[layer]].source` — one row per artifact, read under the names `fields` resolved.
    File { path: PathBuf, fields: Fields },
    /// `[[layer]].artifacts` — the rows themselves, on the canonical names.
    Inline(Vec<InlineArtifact>),
}

/// `[layer.members].source`, and where its fields sit in it.
#[derive(Debug, Clone)]
pub struct MemberSource {
    pub path: PathBuf,
    pub fields: Fields,
}

/// One declared coordinate system.
#[derive(Debug, Clone)]
pub struct View {
    pub name: String,
    /// ⊘ Recorded and not yet published. A title is presentation metadata on an object whose
    /// visibility is already decided, so serving it discloses nothing a name does not — but
    /// `MANIFEST.json` carries no slot for one on a view, an attribute or a vocabulary, and adding
    /// three is a contracts change rather than a declaration one. A **level's** title is published
    /// today, and a **layer's** is.
    pub title: Option<String>,
    /// This view's geometry: `entity_id` with either `x`/`y` or `morton`/`residual`. `None` when
    /// the view declares no source, which is legal to *declare* and refused at a build that would
    /// have to read it.
    pub source: Option<PathBuf>,
    /// Where the identity and geometry fields sit in that file. Canonical is `entity_id` with
    /// either `x`/`y` or `morton`/`residual`.
    pub fields: Fields,
    /// The frame every position in this view is quantised across (`configuration.md` §1).
    /// [`Extent::Auto`] still needs the data: [`resolve_extent`] turns it into [`Bounds`].
    pub extent: Extent,
    /// Where each point's own access label is, and what a point carrying none gets.
    pub point_visibility: PointVisibility,
}

/// A view's quantisation frame, as declared (`configuration.md` §1).
///
/// **Declared on the view rather than passed at invocation**, because a coordinate is quantised
/// across it into 32 bits and quantisation *clamps*: two bundles built from one corpus under
/// different extents place the same point in different cells, and both are well-formed with the
/// geometry wrong. There is deliberately **no constant for the full float range** — spanning
/// ±3.4×10³⁸ over 65,536 cells makes each cell 10³⁴ wide, so every real dataset lands in one of
/// them; it avoids clamping by destroying all resolution.
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum Extent {
    /// `"auto"`, or `{ auto = true, margin = f }` — the **square** box around this build's own
    /// data, with `margin` of the data span added on each side. Resolved by [`resolve_extent`].
    Auto { margin: f64 },
    /// `{ min, max }` or `{ x = [a, b], y = [c, d] }` — stated outright, and the only form a
    /// corpus that will be written to should rely on.
    Fixed(Bounds),
}

/// `auto`'s margin when none is written: 1% of the data span on each side.
///
/// **Not zero**, and the reason is arithmetic rather than taste: the extent is a half-open
/// interval, so a point sitting exactly at the maximum quantises to the clamp. One percent is
/// small enough that a caller who wanted a tight fit has still got one and large enough that the
/// boundary point is inside it. A corpus that will *grow* needs a real margin, and says so.
pub const DEFAULT_AUTO_MARGIN: f64 = 0.01;

/// The share of a view's points that may sit on the frame's boundary before the build refuses
/// rather than reports.
///
/// **Half, and the argument is what a clamped point *is*.** A clamped point's stored position is
/// not its own — it is the frame's — so a frame that misplaces the majority of a corpus is not
/// that corpus's frame; it describes some other data. Below half a clamp is a tail (outliers, a
/// margin left for growth, a deliberately generous box) and the caller may well mean it, which is
/// why the report is unconditional and only this is a refusal. There is no escape flag: the extent
/// quantises, it never filters, so a frame chosen to *crop* piles the rest of the corpus onto the
/// border instead of excluding it — filtering the source is what that caller wants.
pub const CLAMP_REFUSAL_FRACTION: f64 = 0.5;

/// The frame a view is quantised against, beside what the data actually does inside it.
///
/// **The two travel together because neither is readable alone.** An extent is four numbers that
/// look plausible whatever the corpus holds; a data box is four numbers with nothing to be right or
/// wrong against. The failure this exists for shipped a degenerate map from exactly that gap — a
/// grid-shaped extent over UMAP coordinates spanning about −17…18, every point folded into a
/// nineteen-cell corner, and the build silent.
#[derive(Debug, Clone)]
pub struct Frame {
    pub view: String,
    /// What every stored position in this view is quantised across.
    pub extent: Bounds,
    /// The data's own box and the frame's effect on it. [`PointSurvey::Quantised`] for a Morton
    /// source, which arrives already placed.
    pub survey: PointSurvey,
}

impl Frame {
    /// The rows this frame misplaces, or `None` where nothing was quantised.
    fn coordinates(&self) -> Option<&CoordinateSurvey> {
        match &self.survey {
            PointSurvey::Coordinates(survey) => Some(survey),
            PointSurvey::Quantised => None,
        }
    }

    /// **What the build says about this frame, every time, whether or not anything is wrong.**
    /// The extent, the data's own bounds beside it, how much of the grid that leaves the data
    /// occupying, and how many points land on the boundary rather than where they were written.
    ///
    /// Reported rather than merely available: the whole defect this closes was a build that had
    /// every one of these numbers and printed none of them.
    pub fn report(&self) -> String {
        let e = &self.extent;
        let mut out = format!(
            "view '{}': quantising against x [{}, {}], y [{}, {}]",
            self.view, e.x_min, e.x_max, e.y_min, e.y_max
        );
        let Some(survey) = self.coordinates() else {
            out.push_str(
                "\n        points arrive as Morton codes, already placed in this frame — nothing \
                 is quantised here and nothing clamps",
            );
            return out;
        };
        let Some(data) = survey.bounds else {
            out.push_str("\n        the source selects no rows, so nothing was placed");
            return out;
        };
        // Cells, not proportions: 65,536 per axis is the resolution a view actually has, and
        // "the data occupies 19 of them" is the sentence the degenerate map needed.
        let cells_x = u32::from(cell(data.x_max, e.x_min, e.x_max))
            - u32::from(cell(data.x_min, e.x_min, e.x_max))
            + 1;
        let cells_y = u32::from(cell(data.y_max, e.y_min, e.y_max))
            - u32::from(cell(data.y_min, e.y_min, e.y_max))
            + 1;
        out.push_str(&format!(
            "\n        the data spans x [{}, {}], y [{}, {}] — {cells_x} x {cells_y} of the \
             65536 x 65536 cells",
            data.x_min, data.x_max, data.y_min, data.y_max
        ));
        if survey.clamped == 0 {
            out.push_str(&format!(
                "\n        {} point(s) placed, none on the frame's edge",
                survey.rows
            ));
        } else {
            out.push_str(&format!(
                "\n        {} of {} point(s) ({:.1}%) CLAMP onto the frame's edge — {} on x, {} \
                 on y. A clamped point is stored at the boundary, not where it was written",
                survey.clamped,
                survey.rows,
                survey.clamped_fraction() * 100.0,
                survey.clamped_x,
                survey.clamped_y,
            ));
        }
        out
    }

    /// The refusal this frame earns, if any: past [`CLAMP_REFUSAL_FRACTION`] the frame is not
    /// this corpus's frame, and building would write a bundle that is well-formed with the
    /// geometry wrong.
    pub fn refusal(&self) -> Option<String> {
        let survey = self.coordinates()?;
        if survey.clamped_fraction() <= CLAMP_REFUSAL_FRACTION {
            return None;
        }
        let data = survey.bounds?;
        Some(format!(
            "view '{}': {} of {} point(s) ({:.1}%) would be stored on the frame's edge rather \
             than where they were written. The frame is x [{}, {}], y [{}, {}]; the data spans x \
             [{}, {}], y [{}, {}]. Past half the corpus this is not a tail, it is the wrong frame \
             — quantisation clamps rather than filters, so a bundle built here is well-formed \
             with the geometry wrong. Write `extent = \"auto\"` to fit the data, or state the box \
             the data is actually in; filter the source if the intent was to crop",
            self.view,
            survey.clamped,
            survey.rows,
            survey.clamped_fraction() * 100.0,
            self.extent.x_min,
            self.extent.x_max,
            self.extent.y_min,
            self.extent.y_max,
            data.x_min,
            data.x_max,
            data.y_min,
            data.y_max,
        ))
    }
}

/// Resolve a view's declared [`Extent`] into the frame this build quantises against, **and survey
/// what that frame does to the data** in the same pass.
///
/// `Fixed` is already the frame; the pass is what establishes how much of the corpus it clamps.
/// `Auto` reads `points` — the view's own geometry source — and fits a **square** box around it:
/// fitting each axis tightly would use the grid better and silently stretch the map, which is a
/// rendering decision a build has no business making. The margin is a fraction of that square's
/// span, added on each side.
///
/// **One pass either way**, which is what makes the clamp report affordable at every build rather
/// than a cost a caller avoids by stating their extent by hand — which is the caller this exists
/// for.
///
/// The refusals here are the ones `auto` cannot answer for itself: an empty selection frames
/// nothing, and a Morton points file carries no coordinates to frame (that one is refused by
/// [`crate::input::survey_points`], naming the extent to write instead).
pub fn frame_view(
    view: &str,
    extent: &Extent,
    points: &Path,
    fields: &Fields,
    limit: Option<u64>,
) -> Result<Frame> {
    let margin = match extent {
        Extent::Fixed(bounds) => {
            let survey = crate::input::survey_points(points, fields, limit, Some(bounds))?;
            return Ok(Frame {
                view: view.to_string(),
                extent: *bounds,
                survey,
            });
        }
        Extent::Auto { margin } => *margin,
    };
    let survey = crate::input::survey_points(points, fields, limit, None)?;
    let PointSurvey::Coordinates(survey) = survey else {
        unreachable!("survey_points refuses a Morton source when no frame is supplied")
    };
    let data = survey.bounds.ok_or_else(|| {
        declaration_error(format!(
            "view '{view}': `extent` is `auto` and the points source selects no rows, so there is \
             no data to fit a box around. Either the source is empty or `--limit` excludes every \
             row; state the frame instead — `extent = {{ min = <a>, max = <b>}}` — if this corpus \
             is meant to start empty and be written to"
        ))
    })?;
    // Square, then margin: a circle in the data stays a circle on the grid. `span` is the larger
    // of the two axes, and a corpus whose points are all at one position has no span at all — a
    // unit box is the only non-degenerate frame available, and it is centred on the point.
    let span_x = data.x_max - data.x_min;
    let span_y = data.y_max - data.y_min;
    let span = span_x.max(span_y);
    let span = if span > 0.0 { span } else { 1.0 };
    let half = span / 2.0 + span * margin;
    let (cx, cy) = (
        (data.x_min + data.x_max) / 2.0,
        (data.y_min + data.y_max) / 2.0,
    );
    let bounds = Bounds {
        // **Widened to the data it was fitted to, which the arithmetic above does not guarantee.**
        // Centre and half-span are each rounded, so at `margin = 0` the fitted edge can land an ulp
        // inside the data and clamp the extreme row. A no-op for every non-degenerate margin, and
        // what lets `auto` report *no clamps* as a fact rather than as an expectation.
        x_min: (cx - half).min(data.x_min),
        x_max: (cx + half).max(data.x_max),
        y_min: (cy - half).min(data.y_min),
        y_max: (cy + half).max(data.y_max),
    };
    bounds.validate().map_err(|detail| {
        declaration_error(format!(
            "view '{view}': `extent = \"auto\"` fitted no usable box around the data \
             ({detail}). The data spans x [{}, {}], y [{}, {}]; state the frame outright if that \
             is not what this corpus is",
            data.x_min, data.x_max, data.y_min, data.y_max
        ))
    })?;
    Ok(Frame {
        view: view.to_string(),
        extent: bounds,
        survey: PointSurvey::Coordinates(survey),
    })
}

/// Compile a view's `extent`, in any of `configuration.md` §1's four spellings.
///
/// Every refusal names all four, because the key has no default and the value an absent line
/// would supply is a decision about where every stored point lands.
fn compile_extent(view: &str, value: Option<&toml::Value>) -> Result<Extent> {
    let Some(value) = value else {
        return Err(declaration_error(format!(
            "view '{view}': `extent` is required and has no default (configuration.md §1). It is \
             the frame every stored position is quantised across, and quantisation clamps — so a \
             guessed frame is a bundle that is well-formed with the geometry wrong. Four \
             spellings:{}",
            EXTENT_SPELLINGS
        )));
    };
    if let Some(word) = value.as_str() {
        if word == "auto" {
            return Ok(Extent::Auto {
                margin: DEFAULT_AUTO_MARGIN,
            });
        }
        return Err(declaration_error(format!(
            "view '{view}': `extent = \"{word}\"` is not a value this key takes. The only word \
             it takes is `auto`; every other spelling is a table:{}",
            EXTENT_SPELLINGS
        )));
    }
    let Some(table) = value.as_table() else {
        return Err(declaration_error(format!(
            "view '{view}': `extent` is neither the word `auto` nor a table:{}",
            EXTENT_SPELLINGS
        )));
    };
    let table: ExtentTable = ExtentTable::deserialize(toml::Value::Table(table.clone()))
        .map_err(|e| declaration_error(format!("view '{view}': `extent`: {e}")))?;

    let stated =
        table.min.is_some() || table.max.is_some() || table.x.is_some() || table.y.is_some();
    if let Some(auto) = table.auto {
        if !auto {
            return Err(declaration_error(format!(
                "view '{view}': `extent = {{ auto = false }}` says what the frame is not. Write \
                 the frame:{}",
                EXTENT_SPELLINGS
            )));
        }
        if stated {
            return Err(declaration_error(format!(
                "view '{view}': `extent` declares `auto` and a stated box together. `auto` fits \
                 the box to the data this build reads; `min`/`max` and `x`/`y` state it outright. \
                 One or the other:{}",
                EXTENT_SPELLINGS
            )));
        }
        let margin = match table.margin {
            None => DEFAULT_AUTO_MARGIN,
            Some(margin) => {
                if !margin.is_finite() || margin < 0.0 {
                    return Err(declaration_error(format!(
                        "view '{view}': `extent.margin = {margin}` is not a fraction of the data \
                         span. It is headroom added on each side, so it is finite and at least 0 \
                         — a negative margin would shrink the box inside the data and clamp the \
                         points it excluded"
                    )));
                }
                margin
            }
        };
        return Ok(Extent::Auto { margin });
    }
    if table.margin.is_some() {
        return Err(declaration_error(format!(
            "view '{view}': `extent.margin` without `auto = true`. A margin is headroom around a \
             box that was fitted to data; a box stated outright already includes whatever headroom \
             its author wanted:{}",
            EXTENT_SPELLINGS
        )));
    }
    let bounds = match (table.min, table.max, table.x, table.y) {
        (Some(min), Some(max), None, None) => Bounds {
            x_min: min,
            x_max: max,
            y_min: min,
            y_max: max,
        },
        (None, None, Some([x_min, x_max]), Some([y_min, y_max])) => Bounds {
            x_min,
            x_max,
            y_min,
            y_max,
        },
        (None, None, None, None) => {
            return Err(declaration_error(format!(
                "view '{view}': `extent` is an empty table, so it declares no frame at all:{}",
                EXTENT_SPELLINGS
            )))
        }
        (min, max, x, y) => {
            let named = [
                min.map(|_| "min"),
                max.map(|_| "max"),
                x.map(|_| "x"),
                y.map(|_| "y"),
            ]
            .into_iter()
            .flatten()
            .collect::<Vec<_>>()
            .join(", ");
            return Err(declaration_error(format!(
                "view '{view}': `extent` names {named}, which is half a frame. `min` and `max` \
                 give one range to both axes and preserve the aspect ratio; `x` and `y` give a \
                 range each, where stretching is meant. Neither half stands alone:{}",
                EXTENT_SPELLINGS
            )));
        }
    };
    bounds
        .validate()
        .map_err(|detail| declaration_error(format!("view '{view}': `extent`: {detail}")))?;
    Ok(Extent::Fixed(bounds))
}

/// The four spellings, appended to every `extent` refusal. A caller who got this key wrong is
/// choosing between four shapes, not correcting a typo, so the whole set travels with the message.
const EXTENT_SPELLINGS: &str = "\n  \
     extent = \"auto\"                          # the square box around the data, small margin\n  \
     extent = { auto = true, margin = 0.25 }  # a quarter of the data span as headroom each side\n  \
     extent = { min = -25.0, max = 25.0 }     # one range, both axes — preserves aspect ratio\n  \
     extent = { x = [-18, 19], y = [-22, 24] }  # per axis, where stretching is meant";

/// Where a build reads each point's access terms, and what a point carrying none is given.
///
/// **The three shapes are one declaration, not three routes.** `default` is required on all of
/// them (`configuration.md` §1): a point's label has to come from somewhere, and *nowhere* is a
/// decision rather than an omission — so the acquisition half is what is optional, and a corpus
/// with no permission model is the one that declares only a default.
#[derive(Debug, Clone)]
pub struct AccessInput {
    pub source: AccessSource,
    /// What a point carrying no terms of its own is given. Never `inherited` (§1); any other
    /// string is a term, commas and all — the plugin is handed a list, so nothing splits it.
    pub default: String,
}

/// The acquisition half of [`AccessInput`].
#[derive(Debug, Clone)]
pub enum AccessSource {
    /// `point_visibility.source`: a separate exploded `(entity_id, term_id)` relation, one row per
    /// `(point, term)`. The shape the probe generators produce natively at 10⁹.
    Relation(PathBuf),
    /// `point_visibility.field`: a `list<string>` — or a plain `string`, where a point carries one
    /// term — of the view's own source.
    Field(String),
    /// Neither: every point takes the default.
    Default,
}

impl AccessInput {
    /// The exploded relation, with `public` as the default — the shape every fixture that predates
    /// the field route declares, spelled once here rather than at each of them.
    pub fn relation(path: impl Into<PathBuf>) -> AccessInput {
        AccessInput {
            source: AccessSource::Relation(path.into()),
            default: String::from_utf8(tessera_authz::PUBLIC_LABEL.to_vec())
                .expect("the reserved label is ASCII"),
        }
    }
}

/// A view's `point_visibility = { field, default }` or `{ source, default }`.
#[derive(Debug, Clone)]
pub struct PointVisibility {
    /// A column of the view's own source, carrying one label or a list per point.
    pub field: Option<String>,
    /// The exploded `(entity_id, term_id)` relation, bound.
    pub source: Option<PathBuf>,
    /// **Never `inherited`.** A point carrying no terms is in no posting list and so in no
    /// principal's mask, and a gate narrows rather than widens — so there is nothing for a point to
    /// inherit, and the word is refused at parse for points where it is legal for artifacts.
    pub default: String,
}

/// The attributes in declaration order, and the vocabularies they reference.
///
/// Declaration order is load-bearing and not a convenience. The scalar tail is stored and read
/// back **positionally** — `columns.arrow`'s schema is the fixed columns followed by this list,
/// and `/control/ingest` builds each row's vector in declared order — so reordering the file
/// reorders the columns of every segment built after it.
#[derive(Debug, Clone, Default)]
pub struct Schema {
    pub attributes: Vec<Attribute>,
    /// Keyed by vocabulary name. Several attributes may share one by naming it: keys, codes and
    /// properties are shared, membership is not.
    pub vocabularies: HashMap<String, Vocabulary>,
}

/// One declared attribute, with its placement derived (records §2).
#[derive(Debug, Clone)]
pub struct Attribute {
    pub name: String,
    /// ⊘ Recorded and not yet published — see [`View::title`].
    pub title: Option<String>,
    /// The column this attribute is read from, where it differs from the served name. `None` means
    /// the two are the same; [`Attribute::column`] is what a reader asks.
    pub field: Option<String>,
    /// The declared type. For a category this is the **vocabulary's** width, which is why two
    /// attributes sharing a vocabulary can no longer disagree about it: the disagreement is not
    /// expressible rather than refused (`per-point-attributes.md` §3.9).
    pub ty: ScalarType,
    /// The analyser producing this column's terms, resolved to its full `<name>/<version>`
    /// identity at parse; `Some` **iff** the type is `text` (decision 0070).
    ///
    /// Resolved here rather than at index time so a config naming an analyser this binary does not
    /// carry is refused at the declaration — where the author can read the message — instead of
    /// part-way through a build. It reaches the reader as the manifest's per-column identity, and
    /// changing it rebuilds that column's index and nothing else.
    pub analyser: Option<String>,
    /// The vocabulary this column's values are drawn from, for a category; `None` for a plain
    /// numeric attribute. Names a key in [`Schema::vocabularies`].
    pub vocabulary: Option<String>,
    /// The named vocabulary's [`ValueSet`], cached beside its name so the batch build can decide
    /// whether to mint without a second lookup. `None` iff `vocabulary` is `None`.
    pub value_set: Option<ValueSet>,
    /// Whether this column carries an entity-space filter index (records §3; `filter-index.md`
    /// §2). The compiled form reaches the reader as `MANIFEST.declared_scalars[..].index`.
    ///
    /// Free of [`Attribute::render`]: a column may declare both, and the two homes then answer the
    /// same predicate over different spaces, which is what lets 0068 route on cost.
    pub index: bool,
    /// Whether this column occupies a slot in every row of `columns.arrow`.
    ///
    /// **Load-bearing, not informational.** The hot column's tail is *exactly* the render columns.
    /// An `index`-only column is entity-space and must not appear in it — that is the whole of
    /// §10.3's routing distinction, and it is what lets a `keyword` column be filterable while
    /// `render` on `keyword` stays refused. A build that wrote every declared attribute into the
    /// tail would put a per-row string in the hot column by the back door, at 0.93 GiB per byte
    /// per row per 10⁹.
    pub render: bool,
}

/// Whether an unknown key is refused or minted — [`tessera_types::layer::ValueSet`], one type for
/// a vocabulary and for a layer, re-exported so a compiled declaration reads under one name here.
///
/// On a **vocabulary** open means an unknown key is minted a fresh code, drawn at random from the
/// declared width's unused space by [`tessera_store::vocabulary::VocabularyMinter`] — the same
/// routine ingest uses, so exhaustion is one predicate. Declared values still pin or assign codes
/// exactly as a closed vocabulary's do; the build mints only for keys the declaration does not
/// carry, and an open vocabulary given no values at all is legal and starts empty. On a **layer**
/// it means an unknown member key mints an artifact (`artifacts-from-points.md` §3).
pub use tessera_types::layer::ValueSet;

/// A named value set: keys, their codes, and per-value presentation.
#[derive(Debug, Clone)]
pub struct Vocabulary {
    pub name: String,
    /// ⊘ Recorded and not yet published — see [`View::title`]. A **value's** title is published,
    /// as `MANIFEST.vocabularies[..].values[..].title`.
    pub title: Option<String>,
    pub value_set: ValueSet,
    /// `public` or `derived` (`per-point-attributes.md` §3.8). Recorded and published; **not yet
    /// enforced anywhere**, there being no `/v1/categories` to filter.
    pub visibility: Visibility,
    /// The **code space's** width, which is why it lives here and not on a column
    /// (`per-point-attributes.md` §3.9).
    pub width: ScalarType,
    /// Value key → code, pinned by the caller or assigned by the build — the compiled form does
    /// not distinguish them, because `MANIFEST.json` is the record either way. For an open
    /// vocabulary this is what the declaration carried *before* the build; the codes minted during
    /// the run live in the minter [`Schema::open_minters`] returns.
    pub codes: BTreeMap<String, u32>,
    /// Per-value presentation, keyed as `codes` is. Absent for a value the author gave no title.
    pub titles: BTreeMap<String, String>,
    /// Retired codes, never reassigned (Protobuf's `reserved`).
    pub reserved: Vec<u32>,
}

/// The values a vocabulary declares, however they were spelt: an inline array, an inline
/// `key = code` table, or a bound file.
///
/// **One type for all three**, because the rules in [`check_codes`] are applied to this after they
/// converge, so no spelling can acquire a rule the others lack.
#[derive(Debug, Clone, Default)]
pub struct DeclaredValues {
    /// Value key → code, where the caller pinned one. A key with no entry here is assigned.
    pub codes: BTreeMap<String, u32>,
    /// Declaration order, which is assignment order for the keys that pinned nothing.
    pub order: Vec<String>,
    /// Per-value presentation, keyed as `codes` is; absent for a value given no title.
    pub titles: BTreeMap<String, String>,
}

/// Is the *existence* of a value sensitive (`per-point-attributes.md` §3.8)? A disclosure control,
/// so it never defaults.
///
/// The manifest's own type, re-exported rather than mirrored: the config compiles straight into
/// `MANIFEST.vocabularies`, and a second spelling of a two-variant disclosure control is a second
/// place for `derived` to become `public` in translation. Declaration, discriminant and wire now
/// carry the same two words, so there is no translation left to get wrong.
pub use tessera_store::manifest::Visibility;

impl Attribute {
    /// The column in this attribute's source it is read from: the declared
    /// `field` where the declaration moved it, and the served `name` otherwise.
    ///
    /// **The served name and the source column are two different things**, which is the whole of
    /// why the key exists: the name addresses the column on the wire (`/v1/categories/{column}`)
    /// and in the manifest, and a producer's file is under no obligation to spell it the same way.
    pub fn column(&self) -> &str {
        self.field.as_deref().unwrap_or(&self.name)
    }
}

impl Vocabulary {
    /// The code for `key`, or `None` if this vocabulary does not declare it.
    ///
    /// A caller mapping ingest or build data through this must treat `None` as a refusal, not as
    /// *absent*: the declare-then-use rule exists because a category carries properties and,
    /// through its postings, a visibility consequence, so a typo must not create one.
    pub fn code_of(&self, key: &str) -> Option<u32> {
        self.codes.get(key).copied()
    }
}

impl Config {
    /// Parse `path`. Every path `[sources]` writes resolves **relative to `path`'s own
    /// directory**, and `overrides` — the `--file NAME=PATH` pairs, keyed by the source's own
    /// name — replace one at a time (`configuration.md` §3, §8).
    ///
    /// The fail-closed rules are §8's: a `source` naming no `[sources]` key is a refusal listing
    /// the names that exist, an override naming no key is a refusal too, and an override never
    /// *creates* a source — so a closed vocabulary cannot be opened from the command line.
    pub fn parse(path: &Path, overrides: &HashMap<String, PathBuf>) -> Result<Config> {
        let text = std::fs::read_to_string(path).map_err(|e| BuildError::io(path, e))?;
        let file: ConfigFile = toml::from_str(&text)
            .map_err(|e| declaration_error(format!("{}: {e}", path.display())))?;

        // `path` has a parent unless it is a bare file name, where the document's own directory is
        // the working directory — which is what `Path::new("")` joins to.
        let base = path.parent().unwrap_or(Path::new("")).to_path_buf();
        let sources = Sources::compile(&base, file.sources.as_ref(), overrides)?;
        let defaults = Defaults::compile(file.defaults.as_ref(), &sources)?;
        let views = compile_views(&file.view, &sources, &defaults)?;
        let vocabularies = compile_vocabularies(&file.vocabulary, &sources)?;
        let attributes = compile_attributes(&file.attribute, &vocabularies)?;
        let attribute_sources = compile_attribute_sources(&file.attribute, &sources, &defaults)?;
        let (layers, layer_sources, label_layers) =
            compile_layers(&file.layer, &views, &attributes, &sources)?;

        Ok(Config {
            schema: Schema {
                attributes,
                vocabularies,
            },
            attribute_sources,
            views,
            layers,
            layer_sources,
            label_layers,
        })
    }

    /// The view a build materialises when the invocation names none.
    ///
    /// **One declared view is not a default; it is the only answer.** With several, choosing would
    /// publish a coordinate system nobody asked for — and since two views quantise the same corpus
    /// differently, the bundle would be well-formed and wrong. With none, there is nothing to
    /// build at all.
    pub fn sole_view(&self) -> Result<&str> {
        match self.views.as_slice() {
            [only] => Ok(&only.name),
            [] => Err(declaration_error(
                "the declaration has no `[[view]]` block, so this build has no coordinate system                  to materialise. A view names the geometry source and the frame it is quantised                  against (configuration.md §1)",
            )),
            several => Err(declaration_error(format!(
                "the declaration has {} views and `--view` names none. A build materialises one                  coordinate system: {}. Two views quantise the same corpus differently, so                  choosing one here would produce a bundle that is well-formed and not the one                  asked for",
                several.len(),
                names(several.iter().map(|v| v.name.as_str()))
            ))),
        }
    }

    /// The files this build reads, for the one view it materialises.
    ///
    /// **Where the declaration meets the invocation.** Everything above is route-independent: the
    /// same blocks describe a deployment that never builds (`configuration.md` §2), and a config
    /// declaring no source at all is legal and declares an empty corpus. This is the method that
    /// asks for the files, so it is where *this* build's absences become refusals — and where the
    /// two acquisition routes that are specified and not built say so rather than reading nothing.
    pub fn acquire(&self, view: &str) -> Result<Acquisition> {
        let declared = self.views.iter().find(|v| v.name == view).ok_or_else(|| {
            declaration_error(format!(
                "--view '{view}' names no `[[view]]` block. Declared: {}. The build materialises \
                 one coordinate system and reads its `source`, so a view it cannot find is a build \
                 with no geometry rather than a default one",
                names(self.views.iter().map(|v| v.name.as_str()))
            ))
        })?;
        let points = declared.source.clone().ok_or_else(|| {
            declaration_error(format!(
                "view '{view}': `source` is required to build from a file (configuration.md §1). \
                 It is the path — relative to this config — of this view's geometry: `entity_id` \
                 with either `x`/`y` or `morton`/`residual`. ⊘ Declaring no source is legal and \
                 means the view is declared and empty, which is a bundle with no rows in it (§2) \
                 and is not built"
            ))
        })?;
        let access = AccessInput {
            source: match (
                &declared.point_visibility.source,
                &declared.point_visibility.field,
            ) {
                (Some(path), _) => AccessSource::Relation(path.clone()),
                (None, Some(field)) => AccessSource::Field(field.clone()),
                // Legal, and the corpus with no permission model: every point takes the default
                // (§1). *Nowhere* is the decision the `default` key makes, so nothing is refused
                // here — a build with neither acquisition key reads no relation and writes the one
                // label the declaration named.
                (None, None) => AccessSource::Default,
            },
            default: declared.point_visibility.default.clone(),
        };
        // **Every declared column must have a file by now.** Declaring one with no source is
        // legal (§2) and is the write-path deployment's normal state; a build that would have to
        // read it is where the absence becomes a refusal, naming the columns rather than the block
        // — which is what `[corpus]` could not do, there being one file for all of them.
        let mut carried: Vec<usize> = self
            .attribute_sources
            .iter()
            .flat_map(|s| s.attributes.iter().copied())
            .collect();
        carried.sort_unstable();
        let unsourced: Vec<&str> = (0..self.schema.attributes.len())
            .filter(|i| carried.binary_search(i).is_err())
            .map(|i| self.schema.attributes[i].name.as_str())
            .collect();
        if !unsourced.is_empty() {
            return Err(declaration_error(format!(
                "{} attribute(s) name no `source` and `[defaults]` declares none: {}. The \
                 attribute pass reads each column from the file its source names, joined to the \
                 view's geometry by the entity id, so there is no file for these to be read from. \
                 Name a `[sources]` key on each, or write `[defaults]` with `source = \"<name>\"` \
                 for every column that does not. ⊘ Declaring a column with no source is legal and \
                 means the schema is declared and empty, which is a bundle with no rows in it (§2) \
                 and is not built",
                unsourced.len(),
                names(unsourced.iter().copied())
            )));
        }
        Ok(Acquisition {
            attribute_sources: self.attribute_sources.clone(),
            extent: declared.extent,
            points,
            point_fields: declared.fields.clone(),
            access,
            layers: self.layer_sources.clone(),
        })
    }
}

/// The files one build reads, resolved from the config and any `--file` overrides, plus the
/// frame it quantises against.
#[derive(Debug, Clone)]
pub struct Acquisition {
    /// The built view's `extent`, as declared. [`resolve_extent`] turns [`Extent::Auto`] into
    /// [`Bounds`] by reading [`Acquisition::points`]; every other spelling is already the answer.
    pub extent: Extent,
    /// The declared attributes grouped by the file each is read from — one pass per group, joined
    /// to the view's geometry by the identity column each group names. Empty for an empty schema;
    /// an attribute with no file to read it from is refused at parse.
    pub attribute_sources: Vec<AttributeSource>,
    /// The built view's `source`: identity and geometry.
    pub points: PathBuf,
    /// Where the view's identity and geometry fields sit in that file.
    pub point_fields: Fields,
    /// Where this view's points get their access terms, and what a point carrying none gets.
    pub access: AccessInput,
    /// Each layer's own artifacts and members, in declaration order. **One source per layer**, so
    /// no row anywhere names the layer it belongs to.
    pub layers: Vec<LayerSources>,
}

/// A comma-separated list for a refusal, or `none`.
fn names<'a>(values: impl Iterator<Item = &'a str>) -> String {
    let mut names: Vec<&str> = values.collect();
    names.sort_unstable();
    if names.is_empty() {
        return "none".to_string();
    }
    names.join(", ")
}

impl Schema {
    /// **Bits** this schema adds to every row, and `None` if any counted column is
    /// variable-width.
    ///
    /// The residency figure, **totalled across attributes rather than reported per column** —
    /// several categories are what makes the cost bite, and a per-column table lets each one look
    /// affordable. Bits rather than bytes because a `bool` costs one: rounding it to a byte would
    /// erase the whole reason to declare one.
    ///
    /// Render columns only: the hot column's tail is exactly the render columns, and an
    /// `index`-only or blob-resident column adds nothing to any row — counting it would price
    /// the cheap placements as the expensive one.
    pub fn row_bits(&self) -> Option<u64> {
        self.attributes
            .iter()
            .filter(|a| a.render)
            .map(|a| a.ty.row_bits())
            .sum()
    }

    /// Whether this schema declares any column at all — the empty case being every bundle built
    /// before a config existed, which must stay buildable and byte-identical.
    pub fn is_empty(&self) -> bool {
        self.attributes.is_empty()
    }

    /// One live [`VocabularyMinter`] per **open** vocabulary, seeded from whatever it already
    /// carries — the declaration's codes, pinned or assigned, plus `reserved`. A closed vocabulary
    /// mints nothing and has no entry here at all, so `input::scan_attributes`'s batch-level mint
    /// pre-pass can never reach one.
    ///
    /// The width bounding a vocabulary's draw is the **vocabulary's**, which is where §1 puts it:
    /// it is the code space's width rather than a column's, and taking it from a column was what
    /// made two attributes able to disagree about one code space.
    pub fn open_minters(&self) -> HashMap<String, VocabularyMinter> {
        let mut minters = HashMap::new();
        for vocabulary in self.vocabularies.values() {
            if vocabulary.value_set != ValueSet::Open {
                continue;
            }
            let mut minter = VocabularyMinter::new(
                vocabulary.name.clone(),
                tessera_store::manifest::VocabularyKind::Discovered,
                vocabulary.visibility,
                vocabulary.width,
            );
            for (key, &code) in &vocabulary.codes {
                minter
                    .seed_value(key, code)
                    .expect("check_codes already proved this vocabulary's codes are consistent");
            }
            for &code in &vocabulary.reserved {
                minter.seed_reserved(code);
            }
            minters.insert(vocabulary.name.clone(), minter);
        }
        minters
    }
}

// ---------------------------------------------------------------------------------------------
// Acquisition, refused rather than ignored
// ---------------------------------------------------------------------------------------------

/// `[sources]` — the caller's own names for the files this declaration reads, each resolved
/// against the directory that declares them and each overridable by `--file NAME=PATH`
/// (`configuration.md` §3, §8).
///
/// **A name, not a path repeated.** Every `source` elsewhere in the document names one of these
/// keys, so a file three blocks read is written once and moved once. The keys are the caller's own
/// words rather than an object path the parser synthesises: `points`, `geometry`, `scores` are
/// what the declaration is about, and `view:s0:point_visibility` was what the parser happened to
/// call one of its readers.
///
/// **There is no name-or-path fallback.** A `source` that names no key here is a refusal listing
/// the names that exist, because the alternative — reading an unmatched name as a relative path —
/// makes a typo a file that does not exist rather than a declaration that does not resolve, and
/// the message then comes from a file reader instead of from the declaration.
///
/// **A path relative to the document, and an absolute one refused.** What must not appear is a
/// machine-specific path: a config carrying `/mnt/scratch/…` describes a corpus that exists on one
/// machine, and `--file` is where that path belongs — on the command line that knows about the
/// machine.
///
/// **`--file NAME=PATH` moves everything reading that source at once**, which is the point of
/// naming them: the previous shape keyed an override by the *object*, so staging one file that
/// three objects read meant three overrides and missing one left that object quietly reading the
/// old file. Two rules survive from it, both fail-closed: an override naming no key in `[sources]`
/// is a refusal listing the keys that exist, and an override never *creates* a source — it
/// replaces a path the declaration already wrote.
struct Sources {
    /// Every declared name, resolved: the override where one was given, otherwise the declared
    /// path joined to the document's own directory.
    paths: BTreeMap<String, PathBuf>,
}

impl Sources {
    /// Resolve `[sources]` against the declaring document's directory and the `--file` overrides.
    fn compile(
        base: &Path,
        declared: Option<&BTreeMap<String, String>>,
        overrides: &HashMap<String, PathBuf>,
    ) -> Result<Sources> {
        let mut paths = BTreeMap::new();
        for (name, source) in declared.into_iter().flatten() {
            if name.trim().is_empty() {
                return Err(declaration_error(
                    "[sources]: a source with an empty name. The key is the name every `source` \
                     in this declaration writes to reach the file",
                ));
            }
            if source.trim().is_empty() {
                return Err(declaration_error(format!(
                    "[sources].{name} is empty, so it names no file. It is a path to the file, \
                     relative to this config; drop the entry if nothing reads it"
                )));
            }
            let declared = Path::new(source);
            if declared.is_absolute() {
                return Err(declaration_error(format!(
                    "[sources].{name} = \"{source}\" is an absolute path. A source is written \
                     relative to this config so it travels in git with the declaration around it. \
                     Move the file beside the config and name it relatively, or override this one \
                     on the command line: `--file {name}={source}`"
                )));
            }
            paths.insert(name.clone(), base.join(declared));
        }
        // Sorted, so a command line with two unmatched names refuses on the same one every time:
        // a message that varies with a hash order is a message an operator cannot compare.
        let mut given: Vec<(&String, &PathBuf)> = overrides.iter().collect();
        given.sort_unstable_by(|a, b| a.0.cmp(b.0));
        for (name, path) in given {
            // **An override replaces a path; it never writes a new name.** A key `[sources]` does
            // not declare would otherwise leave every `source` in the document pointing where it
            // always did, under a command line asking for a different file — the one failure an
            // override must not have.
            if !paths.contains_key(name) {
                return Err(declaration_error(format!(
                    "--file '{name}=…' names no source in this declaration. `--file` overrides a \
                     path `[sources]` already writes, keyed by the source's own name: {}. It never \
                     creates one — a source that exists only on the command line would be a corpus \
                     the declaration does not describe",
                    names(paths.keys().map(String::as_str))
                )));
            }
            paths.insert(name.clone(), path.clone());
        }
        Ok(Sources { paths })
    }

    /// The file `source` names, or a refusal listing the names that exist. `object` is how the
    /// declaration that named it is quoted back.
    fn path(&self, object: &str, source: &str) -> Result<PathBuf> {
        if source.trim().is_empty() {
            return Err(declaration_error(format!(
                "{object}: `source` is empty. It names a key of `[sources]`, which is where the \
                 path lives; omit it to declare the object with no data"
            )));
        }
        self.paths.get(source).cloned().ok_or_else(|| {
            declaration_error(format!(
                "{object}: `source = \"{source}\"` names no key in `[sources]`. Declared: {}. A \
                 source is a name rather than a path — a name this table does not carry is refused \
                 rather than read as a relative path, which would turn a typo into a missing file \
                 instead of a declaration that does not resolve",
                names(self.paths.keys().map(String::as_str))
            ))
        })
    }
}

/// `[defaults]` — the source and the identity column a block takes when it names neither.
///
/// **Two defaults, and they reach different blocks on purpose.** `entity_id_field` reaches every
/// source read under the canonical `entity_id`: it says how this caller spells identity, and a
/// corpus does not spell it three ways across three files. `source` reaches only the two blocks
/// whose absent source is *nothing at all* — a `[[view]]`'s geometry and an `[[attribute]]`'s
/// column, each of which a build has to read from somewhere. It deliberately does **not** reach a
/// vocabulary, a layer, a `[layer.members]` block or a `point_visibility`, because there an absent
/// source is itself a declaration: a vocabulary that mints rather than reads, a layer declared and
/// empty, a membership that is not stored, labels that ride the points' own column. Filling one of
/// those in would turn a declaration into an acquisition nobody wrote.
#[derive(Debug, Clone)]
struct Defaults {
    /// The `[sources]` name, already checked to exist.
    source: Option<String>,
    /// The column an entity id is read from. `entity_id` where the declaration says nothing.
    entity_id_field: String,
}

impl Defaults {
    fn compile(block: Option<&DefaultsBlock>, sources: &Sources) -> Result<Defaults> {
        let Some(block) = block else {
            return Ok(Defaults {
                source: None,
                entity_id_field: ENTITY_ID.to_string(),
            });
        };
        if let Some(source) = &block.source {
            // Checked here rather than where it is taken, so a `[defaults]` naming nothing is one
            // refusal quoting `[defaults]` instead of the same refusal quoting every block that
            // took it.
            sources.path("[defaults]", source)?;
        }
        let entity_id_field =
            match block.entity_id_field.as_deref() {
                None => ENTITY_ID.to_string(),
                Some(field) if field.trim().is_empty() => return Err(declaration_error(
                    "[defaults]: `entity_id_field` is empty, so it names no column. Omit it to \
                     read the entity id under its own name, `entity_id`",
                )),
                Some(field) => field.to_string(),
            };
        Ok(Defaults {
            source: block.source.clone(),
            entity_id_field,
        })
    }
}

/// One field an object's `fields` map may name.
///
/// **The map says *where*, never *whether*** (`configuration.md` §7): the object's own keys assert
/// that a field exists, and the map only locates it. So a field is *known* by being in this table
/// and *declared* by whatever key asserts it — and naming an undeclared one is refused rather than
/// read as the declaration it is not.
struct KnownField {
    name: &'static str,
    /// `None` when this object always has the field; `Some(why)` when it does not have it here,
    /// `why` naming the key that would declare one.
    undeclared: Option<String>,
}

impl KnownField {
    /// A field the object always has.
    fn always(name: &'static str) -> KnownField {
        KnownField {
            name,
            undeclared: None,
        }
    }

    /// A field another key asserts the existence of: present when `declared`, and refused with
    /// `why` when it is not.
    fn asserted_by(name: &'static str, declared: bool, why: &str) -> KnownField {
        KnownField {
            name,
            undeclared: (!declared).then(|| why.to_string()),
        }
    }
}

/// The source-field names one object reads: whatever its `fields` map moved, and the canonical
/// name for everything it did not.
///
/// **Resolved once, at parse, and carried to the reader** — which is what makes the map real
/// rather than decorative. The readers ask this for a name and never for a canonical one, so a
/// field the declaration moved is read from where it was moved to, and a field it left alone is
/// read from the name `configuration.md` §8 gives it.
/// The object is carried with the names because it is half of the refusal: *which declaration*
/// asked for a column the file does not carry is the part a caller acts on, and a reader deep in a
/// Parquet decode has no other way to know it.
#[derive(Debug, Clone, Default)]
pub struct Fields {
    object: String,
    map: BTreeMap<String, String>,
}

impl Fields {
    /// Canonical names throughout, for an object whose declaration moved nothing.
    pub fn canonical(object: impl Into<String>) -> Fields {
        Fields {
            object: object.into(),
            map: BTreeMap::new(),
        }
    }

    /// A map built outright rather than parsed — for a caller binding a reader programmatically,
    /// and for the tests that exercise a moved name without a document around it.
    pub fn moved<K: Into<String>, V: Into<String>>(
        object: impl Into<String>,
        entries: impl IntoIterator<Item = (K, V)>,
    ) -> Fields {
        Fields {
            object: object.into(),
            map: entries
                .into_iter()
                .map(|(k, v)| (k.into(), v.into()))
                .collect(),
        }
    }

    /// The declaration these names came from, for a refusal to name.
    pub fn object(&self) -> &str {
        if self.object.is_empty() {
            "this source"
        } else {
            &self.object
        }
    }

    /// The column `canonical` is read from.
    pub fn of<'a>(&'a self, canonical: &'a str) -> &'a str {
        self.map
            .get(canonical)
            .map(String::as_str)
            .unwrap_or(canonical)
    }

    /// Whether the declaration named this field at all.
    ///
    /// **An assertion, not a location.** A reader choosing between two mutually exclusive shapes —
    /// a view's `x`/`y` against its `morton`/`residual` — reads this as *the caller says the file
    /// has one of these*, so a named-but-absent column becomes a refusal rather than a silent fall
    /// through to the other shape.
    pub fn names(&self, canonical: &str) -> bool {
        self.map.contains_key(canonical)
    }
}

/// Check one object's `fields` map — every name known, every name declared — and resolve it.
///
/// `entity_id` is where this declaration spells the identity column — `[defaults]`'s, or the
/// canonical name. **Folded into the resolved map rather than consulted by the reader**, so an
/// object whose own map moves `entity_id` keeps its own answer and every reader below this asks
/// one question instead of two.
fn check_fields(
    object: &str,
    source: Option<&PathBuf>,
    known: &[KnownField],
    map: Option<&BTreeMap<String, String>>,
    entity_id: &str,
) -> Result<Fields> {
    let takes_entity_id = known.iter().any(|f| f.name == ENTITY_ID);
    let Some(map) = map else {
        let mut fields = Fields::canonical(object);
        if takes_entity_id && entity_id != ENTITY_ID {
            fields
                .map
                .insert(ENTITY_ID.to_string(), entity_id.to_string());
        }
        return Ok(fields);
    };
    // A map with no source names the fields of nothing. Refused rather than kept for a source that
    // may arrive later: the object reads no file at all, so every entry in it is inert.
    if source.is_none() {
        return Err(declaration_error(format!(
            "{object}: `fields` without a `source`. The map locates this object's fields in the \
             file its source names, and this object names none — so there is no file for the names \
             to be read out of"
        )));
    }
    for (canonical, actual) in map {
        let Some(field) = known.iter().find(|f| f.name == canonical.as_str()) else {
            return Err(declaration_error(format!(
                "{object}: `fields.{canonical}` is not one of this object's fields. They are: {}. \
                 The map says where a field is and never whether there is one, so a name outside \
                 the set is refused rather than passed to the reader",
                names(known.iter().map(|f| f.name))
            )));
        };
        if let Some(why) = &field.undeclared {
            return Err(declaration_error(format!(
                "{object}: `fields.{canonical}` names a field this object never declared — {why}. \
                 `fields` says *where* a field is, never *whether* there is one, so locating one \
                 nothing declared is refused rather than read as the declaration"
            )));
        }
        if actual.trim().is_empty() {
            return Err(declaration_error(format!(
                "{object}: `fields.{canonical}` is empty, so it names no column. Omit the entry to \
                 read `{canonical}` under its own name"
            )));
        }
    }
    let mut resolved = map.clone();
    if takes_entity_id && entity_id != ENTITY_ID {
        // The object's own map wins: `[defaults]` says how this declaration usually spells
        // identity, and a block naming its own column has said otherwise.
        resolved
            .entry(ENTITY_ID.to_string())
            .or_insert_with(|| entity_id.to_string());
    }
    Ok(Fields {
        object: object.to_string(),
        map: resolved,
    })
}

/// Expand every `[layer.labels]` block into a `[[layer]]` block of its own, in place.
///
/// **The sugar is expanded before anything is compiled**, which is what makes it sugar rather than
/// a second implementation: the block below is a [`LayerBlock`] like any other by the time
/// `compile_layers` sees it, so it meets every refusal, every allocator rule and every reader a
/// hand-written layer meets, and the two spellings produce a byte-identical bundle
/// (`annotation-write-cycle.md` §6.1). Nothing downstream of here knows a label layer from a layer.
///
/// The block's `source`, `fields` and `[layer.labels.members]` are `[[layer]]`'s own, and carry
/// straight across. Five things are supplied and one is defaulted:
///
/// * **the parent's `views`** — a label is drawn where the thing it labels is drawn;
/// * **`hierarchy = { kind = "flat" }`** — a label layer is one population, its lineage being the
///   parent's;
/// * **`depends_on = [parent]`**, which is what admits the `attached_layer` / `attached_key` edge
///   every label hangs from;
/// * **the content wrapper** — one `[[layer.content.supplied]]` entry, named for the layer itself
///   and typed by `type`, at `require_member_visibility = "all"`. Fixed rather than taken from the
///   block's own key, and fixed at the strict end: a label is generated from its members, so
///   containment is what its content is served on. A label whose content is true whether or not a
///   document exists — `"inherited"` (C28) — is written out as a `[[layer]]`;
/// * **`artifact_visibility = { default = "inherited" }`** — the sugar names no column to carry a
///   per-artifact label, and with no column the only values expressible are *the layer's gate is
///   the whole of it* and *one fixed label on every artifact*. A label layer whose artifacts carry
///   labels of their own is written out as a `[[layer]]`, where the field can be named;
/// * **`visibility` defaults to the parent's** — the one defaulted disclosure control here.
///
/// ## Why no gate is compared against the parent's
///
/// The expansion once refused a `public` label layer under a gated parent — the one place the sugar
/// was stricter than the two `[[layer]]` blocks it expands to, and escapable by writing them out.
/// **The check is now a property**
/// ([decision 0089](../../../docs/decisions/0089-a-dependency-edge-carries-deletion-and-visibility.md)):
/// a label is served only where the cluster it attaches to is served, so a `public` label layer
/// under a gated parent discloses nothing — a viewer who cannot reach the cluster cannot reach its
/// labels either, whatever the label layer's own gate says. The same rule closes the case a
/// comparison could never have decided: two distinct non-`public` labels carry no ordering the
/// build can compute, because whether every principal holding `ir:secret` also holds `ir:analyst`
/// is a fact about grants, which live outside the bundle entirely. Under rule 2 neither case needs
/// deciding here.
fn expand_labels(blocks: &[LayerBlock]) -> Result<(Vec<LayerBlock>, BTreeMap<String, String>)> {
    let mut expanded: Vec<LayerBlock> = Vec::with_capacity(blocks.len());
    // Which layers the sugar wrote, and for whom. Nothing below this line can tell them from a
    // hand-written `[[layer]]` — that is the property — so the fact is recorded here or nowhere,
    // and `reports/disclosure.json` is where a reviewer reads it back.
    let mut from_labels: BTreeMap<String, String> = BTreeMap::new();
    for block in blocks {
        let mut parent = block.clone();
        let Some(labels) = parent.labels.take() else {
            expanded.push(parent);
            continue;
        };
        let object = format!("layer '{}' `[layer.labels]`", block.name);

        // **The default is the parent's own value**, read from the parent's own declaration rather
        // than from anything compiled: a parent that declares no gate is refused on its own
        // account, and its refusal fires first, the parent being pushed before this block.
        let visibility = match (&labels.visibility, &parent.visibility) {
            (Some(declared), _) => Some(declared.clone()),
            (None, parent_gate) => parent_gate.clone(),
        };

        // **Both of these are declared, never supplied.** A disclosure control has no default,
        // because the value an expansion could pick for a caller who wrote nothing is the value
        // the caller never chose — and here the expansion previously picked both, fixing the
        // content at `all` and the artifact gate at `inherited`. `visibility` remains the single
        // exception in the surface, and only because the value it takes is the parent's own.
        let artifact_visibility = labels.artifact_visibility.clone().ok_or_else(|| {
            declaration_error(format!(
                "{object}: `artifact_visibility` is required — what a label carrying no access                  label of its own is gated on. Write `{{ default = \"inherited\" }}` for labels                  gated with the layer, or name the field carrying each label's own"
            ))
        })?;
        let content_requirement = labels
            .content
            .as_ref()
            .and_then(|c| c.require_member_visibility.clone())
            .ok_or_else(|| {
                declaration_error(format!(
                    "{object}: `[layer.labels.content]` must declare                      `require_member_visibility` — `all` where the label text was generated from                      the documents it names, so a viewer reads a synthesis only of documents it                      can already see, or `inherited` where the text is true whether or not any of                      them exists. Only the caller knows which, and the wider of the two cannot be                      a default"
                ))
            })?;

        let ty = labels.ty.clone().ok_or_else(|| {
            declaration_error(format!(
                "{object}: `type` is required — it is the kind of content each label carries, \
                 published on `/v1/meta` so a client knows what to draw: text, polygon, extent or \
                 point"
            ))
        })?;

        let child = LayerBlock {
            name: labels.name.clone(),
            title: labels.title.clone(),
            members: labels.members.clone(),
            labels: None,
            views: parent.views.clone(),
            source: labels.source.clone(),
            fields: labels.fields.clone(),
            artifacts: None,
            membership: labels.membership.clone(),
            // A label is written out, one row per label, so its artifacts *are* the roster: there
            // is no `value_set` in the sugar's key set and nothing for an open one to mint from.
            value_set: None,
            hierarchy: Some(HierarchyBlock {
                kind: Some("flat".to_string()),
                prune_children: false,
            }),
            // **Not supplied, and not inherited from the parent either.** The sugar carries no key
            // the `[[layer]]` surface does not, and a layout pin is a statement about where *this*
            // layer's data landed — a label layer's population is the parent's label count, which
            // is a different shape from the parent's own membership. Absent, the pick is automatic,
            // which is what a caller who wrote nothing asked for.
            layout: None,
            visibility,
            artifact_visibility: Some(artifact_visibility),
            require_member_visibility: labels.require_member_visibility.clone(),
            withdraw_on_member_deletion: None,
            depends_on: vec![parent.name.clone()],
            levels: Vec::new(),
            // A label layer's members are its own rows; there is no shape in the sugar's key set.
            shape: None,
            content: Some(ContentBlock {
                computed: Vec::new(),
                supplied: vec![SuppliedBlock {
                    name: labels.name.clone(),
                    ty: Some(ty),
                    require_member_visibility: Some(content_requirement),
                }],
                withdraw_on_member_deletion: None,
            }),
        };
        // **Immediately after its parent**, because a layer is declared after every layer it names
        // in `depends_on` and the expansion has just named one.
        from_labels.insert(child.name.clone(), block.name.clone());
        expanded.push(parent);
        expanded.push(child);
    }
    Ok((expanded, from_labels))
}

// ---------------------------------------------------------------------------------------------
// Attribute sources
// ---------------------------------------------------------------------------------------------

/// Group the declared attributes by the file each is read from, and the column each joins on.
///
/// **The grouping is what replaced `[corpus]`.** That block named one file every attribute was
/// read from, and the constraint was arbitrary: what it guaranteed — that every attribute lands in
/// one entity space — is guaranteed by the entity id and never was by the file. So a column may
/// name its own `source` and its own `entity_id_field`, and the build runs the attribute pass once
/// per `(source, identity column)` pair rather than once over one file.
///
/// Groups come out in the order each group's **first** attribute was declared, and each group's
/// indices ascend. That order is not cosmetic anywhere it is read: the scalar tail is stored
/// positionally, so a group is a subset of the declaration order rather than a reordering of it.
fn compile_attribute_sources(
    blocks: &[AttributeBlock],
    sources: &Sources,
    defaults: &Defaults,
) -> Result<Vec<AttributeSource>> {
    let mut groups: Vec<AttributeSource> = Vec::new();
    for (index, block) in blocks.iter().enumerate() {
        let object = format!("attribute '{}'", block.name);
        // **An attribute with no source at all is legal to *declare*** (`configuration.md` §2):
        // a deployment that writes through the service declares its columns and acquires nothing,
        // and the empty bundle is what carries the schema. A build that has to read the column is
        // where that becomes a refusal ([`Config::acquire`]), naming the columns with nowhere to
        // read from.
        let name = match (&block.source, &defaults.source) {
            (Some(declared), _) => declared.clone(),
            (None, Some(fallback)) => fallback.clone(),
            (None, None) => continue,
        };
        let path = sources.path(&object, &name)?;
        let entity_id = match block.entity_id_field.as_deref() {
            None => defaults.entity_id_field.clone(),
            Some(field) if field.trim().is_empty() => {
                return Err(declaration_error(format!(
                    "{object}: `entity_id_field` is empty, so it names no column. Omit it to join \
                     on '{}', which is what this declaration spells the entity id",
                    defaults.entity_id_field
                )))
            }
            Some(field) => field.to_string(),
        };
        match groups
            .iter_mut()
            .find(|g| g.name == name && g.fields.of(ENTITY_ID) == entity_id)
        {
            Some(group) => group.attributes.push(index),
            None => groups.push(AttributeSource {
                name: name.clone(),
                path,
                fields: Fields::moved(
                    format!("source '{name}'"),
                    [(ENTITY_ID.to_string(), entity_id)],
                ),
                attributes: vec![index],
            }),
        }
    }
    Ok(groups)
}

// ---------------------------------------------------------------------------------------------
// Views
// ---------------------------------------------------------------------------------------------

fn compile_views(
    blocks: &[ViewBlock],
    sources: &Sources,
    defaults: &Defaults,
) -> Result<Vec<View>> {
    let mut seen: HashSet<&str> = HashSet::new();
    let mut views = Vec::with_capacity(blocks.len());
    for block in blocks {
        if block.name.is_empty() {
            return Err(declaration_error("a view with an empty name"));
        }
        if !seen.insert(block.name.as_str()) {
            return Err(declaration_error(format!(
                "view '{}' is declared twice. A view name is an identity that is tombstoned on \
                 drop and never reused, so two blocks of one name is not a last-one-wins config \
                 question",
                block.name
            )));
        }
        let object = format!("view '{}'", block.name);
        // **A view with no `source` takes `[defaults].source`**, and a declaration with neither is
        // still legal here: a view declared and empty is the normal state for a deployment that
        // writes through the service (`configuration.md` §2). It becomes a refusal at
        // [`Config::acquire`], where a build asks for the file.
        let source = match &block.source {
            Some(declared) => Some(sources.path(&object, declared)?),
            None => match &defaults.source {
                Some(name) => Some(sources.path(&object, name)?),
                None => None,
            },
        };
        // **The two geometry shapes are mutually exclusive** (§1): a row carries `x`/`y` or
        // `morton`/`residual`, and a map naming one of each says the file carries both — which the
        // reader would resolve by preferring one, silently, over a declaration that asked for the
        // other.
        let fields = check_fields(
            &object,
            source.as_ref(),
            &[
                KnownField::always(ENTITY_ID),
                KnownField::always("x"),
                KnownField::always("y"),
                KnownField::always("morton"),
                KnownField::always("residual"),
            ],
            block.fields.as_ref(),
            &defaults.entity_id_field,
        )?;
        if let Some(fields) = &block.fields {
            let quantised = fields.contains_key("x") || fields.contains_key("y");
            let coded = fields.contains_key("morton") || fields.contains_key("residual");
            if quantised && coded {
                return Err(declaration_error(format!(
                    "view '{}': `fields` names both an `x`/`y` pair and a `morton` code, and the \
                     two geometry shapes are mutually exclusive (configuration.md §1). A row \
                     carries coordinates or a code, so naming both says the source has two \
                     geometries and leaves the reader to pick",
                    block.name
                )));
            }
            if fields.contains_key("residual") && !fields.contains_key("morton") {
                return Err(declaration_error(format!(
                    "view '{}': `fields.residual` without `fields.morton`. A residual is the \
                     sub-cell remainder of a Morton code and is read only beside one",
                    block.name
                )));
            }
        }
        let extent = compile_extent(&block.name, block.extent.as_ref())?;
        if block.visibility.is_some() {
            return Err(declaration_error(format!(
                "view '{}': `visibility` is specified and not built (views §3 — a view's own \
                 gate). A bundle has one coordinate system, so nothing evaluates a per-view gate \
                 yet; accepting it would register a view reachable by everyone under a declaration \
                 saying otherwise",
                block.name
            )));
        }

        let point = block.point_visibility.as_ref().ok_or_else(|| {
            declaration_error(format!(
                "view '{}': `point_visibility` is required and has no default \
                 (configuration.md §1). Write `point_visibility = {{ field = \"<column>\", \
                 default = \"<label>\" }}`: `field` says where each point's own access label is, \
                 and `default` says what a point carrying none gets — `public` reaches every \
                 principal, any other word is an access label. There is no default because the \
                 value an absent line would supply is one of those two, and both are decisions",
                block.name
            ))
        })?;
        if let Some(field) = &point.field {
            if field.trim().is_empty() {
                return Err(declaration_error(format!(
                    "view '{}': `point_visibility.field` is empty. Omit it to say points carry no \
                     labels of their own",
                    block.name
                )));
            }
        }
        // **A point's label comes from a field or from a source, never both** (§1). They are two
        // shapes of one relation — a list per point, or a row per `(point, term)` — so a view
        // declaring both has said the labels are in two places and left the build to choose.
        if point.field.is_some() && point.source.is_some() {
            return Err(declaration_error(format!(
                "view '{}': `point_visibility` declares both a `field` and a `source`, and a \
                 point's label comes from one or the other (configuration.md §1). `field` is a \
                 column of this view's own source, one value or a list per point; `source` is a \
                 separate exploded `(entity_id, term_id)` relation. Declaring both leaves which \
                 one carries a point's terms to the reader",
                block.name
            )));
        }
        let labels = match &point.source {
            Some(declared) => Some(sources.path(&format!("{object} point_visibility"), declared)?),
            None => None,
        };
        let default = point.default.as_deref().ok_or_else(|| {
            declaration_error(format!(
                "view '{}': `point_visibility.default` is required and has no default. It is what \
                 a point carrying no label of its own gets — `public` reaches every principal, and \
                 any other word is an access label. `inherited` is not available here: a point \
                 carrying no terms is in no posting list and so in no principal's mask, so there \
                 is nothing to inherit",
                block.name
            ))
        })?;
        if default == INHERITED {
            return Err(declaration_error(format!(
                "view '{}': `point_visibility.default = \"inherited\"` is refused. A container's \
                 gate narrows rather than widens, and a point carrying no terms is already in no \
                 principal's mask — so inheriting would have to *add* a term to the point, which \
                 can only widen it (configuration.md §4). Name the label such a point should carry, \
                 or `public`",
                block.name
            )));
        }
        check_label(&block.name, "point_visibility.default", default)?;

        views.push(View {
            name: block.name.clone(),
            title: block.title.clone(),
            source,
            fields,
            extent,
            point_visibility: PointVisibility {
                field: point.field.clone(),
                source: labels,
                default: default.to_string(),
            },
        });
    }
    Ok(views)
}

/// A word written where an access label goes. `public` is a label and is fine; `inherited` is the
/// one reserved word occupying such a slot (§4), so it is refused rather than interned.
fn check_label(object: &str, key: &str, label: &str) -> Result<()> {
    if label.trim().is_empty() {
        return Err(declaration_error(format!(
            "'{object}': `{key}` is empty. An access label is a term a principal either holds or \
             does not; write `public` for the one every principal holds"
        )));
    }
    if label == INHERITED {
        return Err(declaration_error(format!(
            "'{object}': an access label may not be spelled `inherited` — it is reserved for *the \
             container's gate is the whole of it*, and it is the one reserved word occupying a \
             slot that otherwise takes a label (configuration.md §4). `public` is not reserved in \
             this sense: it *is* a label, held by every principal"
        )));
    }
    Ok(())
}

// ---------------------------------------------------------------------------------------------
// Vocabularies
// ---------------------------------------------------------------------------------------------

fn compile_vocabularies(
    blocks: &[VocabularyBlock],
    sources: &Sources,
) -> Result<HashMap<String, Vocabulary>> {
    let mut compiled: HashMap<String, Vocabulary> = HashMap::new();
    for block in blocks {
        if block.name.is_empty() {
            return Err(declaration_error("a vocabulary with an empty name"));
        }
        if compiled.contains_key(&block.name) {
            return Err(declaration_error(format!(
                "vocabulary '{}' is declared twice. A vocabulary is an object named by every \
                 attribute that shares it, so two blocks of one name is not a last-one-wins \
                 question — it is two code spaces read back under one name, and which one a stored \
                 code meant would depend on parse order",
                block.name
            )));
        }
        let object = format!("vocabulary '{}'", block.name);
        // **No `[defaults].source` here**: a vocabulary with no source is one that mints rather
        // than reads, and supplying it a file would open a value set nobody opened.
        let source = match &block.source {
            Some(declared) => Some(sources.path(&object, declared)?),
            None => None,
        };
        // A `code` field pins the codes and its absence assigns them, which is why it is *always*
        // available rather than asserted by another key: which of the two a file does is the file's
        // to say, and §1 makes that the one difference between the two spellings.
        let fields = check_fields(
            &object,
            source.as_ref(),
            &[
                KnownField::always("key"),
                KnownField::always("code"),
                KnownField::always("title"),
            ],
            block.fields.as_ref(),
            ENTITY_ID,
        )?;

        let width_name = block.width.as_deref().ok_or_else(|| {
            declaration_error(format!(
                "vocabulary '{}': `width` is required and has no default (configuration.md §6). \
                 The three are u8 (255 usable values), u16 and u32 — it is the code space's width, \
                 baked into every row that carries a value from this vocabulary, so changing it \
                 rewrites the corpus. That is a migration, not a default worth guessing",
                block.name
            ))
        })?;
        let width = ScalarType::parse(width_name)
            .filter(|t| t.is_category_width())
            .ok_or_else(|| {
                declaration_error(format!(
                    "vocabulary '{}': `width = \"{width_name}\"` is not a category width. The \
                     three are u8 (255 usable values), u16 and u32 — per-point-attributes §3.6. \
                     Code 0 is the reserved *absent* sentinel, which is why each carries one fewer \
                     value than its range",
                    block.name
                ))
            })?;

        let value_set = match block.value_set.as_deref() {
            Some("closed") => ValueSet::Closed,
            Some("open") => ValueSet::Open,
            Some(other) => {
                return Err(declaration_error(format!(
                    "vocabulary '{}': `value_set = \"{other}\"` is neither \"closed\" nor \
                     \"open\". `closed` refuses an unknown key at ingest; `open` mints it a fresh \
                     code",
                    block.name
                )));
            }
            None => {
                return Err(declaration_error(format!(
                    "vocabulary '{}': `value_set` is required and has no default \
                     (configuration.md §6). `closed` means the set is authored and an unknown key \
                     at ingest is refused; `open` means an unknown key is minted a fresh code. \
                     There is no default because either answer decides what a typo in a data file \
                     does — create a category, or fail the batch",
                    block.name
                )));
            }
        };

        let visibility = match block.visibility.as_deref() {
            Some("public") => Visibility::Public,
            Some("derived") => Visibility::Derived,
            Some(other) => {
                return Err(declaration_error(format!(
                    "vocabulary '{}': `visibility = \"{other}\"` is neither \"public\" nor \
                     \"derived\", and this slot takes no access label — only a layer's does. \
                     `public` publishes the value set; `derived` makes a value's existence follow \
                     from the viewer being able to see a point carrying it. A word outside the two \
                     is refused rather than read as a label, because reading it as one would gate \
                     the set on a term nobody holds — or publish it",
                    block.name
                )));
            }
            None => {
                return Err(declaration_error(format!(
                    "vocabulary '{}': `visibility` is required and has no default \
                     (configuration.md §6). It is a disclosure control — whether the *existence* \
                     of a value is sensitive — with exactly two settings: `public` publishes the \
                     value set, `derived` makes a value's existence follow from the viewer being \
                     able to see a point carrying it. A bundle built without one would have to be \
                     rebuilt to acquire it",
                    block.name
                )));
            }
        };

        // §3.8's original refusal, relaxed to a warning by owner ruling (2026-08-07): an open
        // vocabulary's values are inferred from whatever is in the corpus, so publishing them
        // discloses data-derived names on nobody's authority — but the operator may have a reason,
        // and there is still no `/v1/categories` for the disclosure to reach.
        if visibility == Visibility::Public && value_set == ValueSet::Open {
            eprintln!(
                "warning: vocabulary '{}': `visibility = \"public\"` with `value_set = \"open\"` \
                 publishes data-derived value names on nobody's authority (per-point-attributes \
                 §3.8, relaxed from a refusal to a warning by owner ruling 2026-08-07). Confirm \
                 this is intended",
                block.name
            );
        }

        let reserved = compile_reserved(block)?;
        let mut declared = match (&block.values, source.as_ref()) {
            (Some(_), Some(_)) => {
                return Err(declaration_error(format!(
                    "vocabulary '{}' declares values inline and names a `source`. They are \
                     spellings of one thing — a value set is inline *or* sourced — so declaring \
                     both is a parse error rather than a precedence question",
                    block.name
                )));
            }
            (Some(inline), None) => parse_inline_values(inline, &block.name)?,
            (None, Some(path)) => crate::input::read_vocabulary_file(path, &block.name, &fields)?,
            (None, None) if value_set == ValueSet::Closed => {
                return Err(declaration_error(format!(
                    "vocabulary '{}': `value_set = \"closed\"` with no value source. A closed set \
                     is authored, and an authored set of nothing refuses every ingest and costs \
                     its width in every row for ever. Declare `values = [\"a\", \"b\"]` (codes \
                     assigned in the order given), or a `[vocabulary.values]` table pinning them, \
                     or name a `source` and bind it with `--file`. An unbound source is never a \
                     silent fall-through to minting, which would open the set with nobody deciding \
                     to",
                    block.name
                )));
            }
            // Open, and no values given: starts empty rather than closing the set, and the build
            // mints every code it will ever carry.
            (None, None) => DeclaredValues::default(),
        };

        assign_codes(&mut declared, &reserved, width, &block.name)?;
        check_codes(&declared.codes, &reserved, width, &block.name)?;

        // **Applied here, after the three spellings converge**, and not at the source: an inline
        // table, a bare key array and a bound Parquet each reach this point as one `codes` map, so
        // no spelling can acquire a rule another lacks. Refusing only *the absence of a source*
        // would admit a source that declares nothing, which is the same column with the same cost
        // and none of the message.
        if value_set == ValueSet::Closed && declared.codes.is_empty() {
            return Err(declaration_error(format!(
                "vocabulary '{}': `value_set = \"closed\"` with no values. A closed set is the \
                 authority on what may be ingested, so an empty one refuses every value for ever \
                 while its column costs its width in every row. Author the values, or write \
                 `value_set = \"open\"` to have them minted as they arrive",
                block.name
            )));
        }

        compiled.insert(
            block.name.clone(),
            Vocabulary {
                name: block.name.clone(),
                title: block.title.clone(),
                value_set,
                visibility,
                width,
                codes: declared.codes,
                titles: declared.titles,
                reserved,
            },
        );
    }
    Ok(compiled)
}

fn compile_reserved(block: &VocabularyBlock) -> Result<Vec<u32>> {
    let mut reserved = Vec::new();
    for raw in block.reserved.iter().flatten() {
        reserved.push(u32::try_from(*raw).map_err(|_| {
            declaration_error(format!(
                "vocabulary '{}': reserved code {raw} is not a u32",
                block.name
            ))
        })?);
    }
    Ok(reserved)
}

/// An inline value set: either an array of keys, whose codes the build assigns in the order given,
/// or a `key = code` table pinning them.
///
/// **Which one was written is the whole of the difference**, and it is not a mode: a caller who
/// does not care which integer a value gets should not have to invent one.
fn parse_inline_values(values: &toml::Value, vocabulary: &str) -> Result<DeclaredValues> {
    let mut set = DeclaredValues::default();
    match values {
        toml::Value::Array(keys) => {
            for entry in keys {
                let key = entry.as_str().ok_or_else(|| {
                    declaration_error(format!(
                        "vocabulary '{vocabulary}': `values` as an array is an array of value \
                         *keys*, and {entry} is not a string. Write \
                         `[vocabulary.values]` with `key = code` to pin the codes instead"
                    ))
                })?;
                if set.order.iter().any(|seen| seen == key) {
                    return Err(declaration_error(format!(
                        "vocabulary '{vocabulary}': value '{key}' is listed twice. Which code it \
                         would take is decided by position, so it is refused"
                    )));
                }
                set.order.push(key.to_string());
            }
        }
        toml::Value::Table(table) => {
            for (key, value) in table {
                let raw = value.as_integer().ok_or_else(|| {
                    declaration_error(format!(
                        "vocabulary '{vocabulary}': value '{key}' must be an integer code, not \
                         {value}. Write `values = [\"{key}\", …]` to have the build assign codes \
                         in the order given"
                    ))
                })?;
                let code = u32::try_from(raw).map_err(|_| {
                    declaration_error(format!(
                        "vocabulary '{vocabulary}': value '{key}' has code {raw}, which is not a \
                         u32"
                    ))
                })?;
                set.order.push(key.clone());
                set.codes.insert(key.clone(), code);
            }
        }
        other => {
            return Err(declaration_error(format!(
                "vocabulary '{vocabulary}': `values` is {other}, and it must be either an array of \
                 keys — codes assigned by the build, in the order given — or a \
                 `[vocabulary.values]` table of `key = code`"
            )));
        }
    }
    Ok(set)
}

/// Give a code to every declared value that pinned none: the lowest free one, in declaration
/// order, skipping `reserved` and the *absent* sentinel.
///
/// **Assignment starts at 1 and never reaches 0**, which is the sentinel — see [`ABSENT_CODE`].
///
/// ⊘ It assigns from an empty slate every build. `configuration.md` §1's carry rule — a rebuild
/// replays the recorded codes, a new value takes the next free one, a removed value's code moves
/// to `reserved` — needs the previous manifest, which nothing reads here yet. So **reordering a
/// bare key list reorders its codes today**; pinning is what holds them still.
fn assign_codes(
    declared: &mut DeclaredValues,
    reserved: &[u32],
    width: ScalarType,
    vocabulary: &str,
) -> Result<()> {
    let max = width
        .max_code()
        .expect("a category width always has a maximum code");
    let mut taken: BTreeSet<u32> = declared.codes.values().copied().collect();
    taken.extend(reserved.iter().copied());
    let mut next = ABSENT_CODE + 1;
    for key in &declared.order {
        if declared.codes.contains_key(key) {
            continue;
        }
        while taken.contains(&next) {
            next += 1;
        }
        if next > max {
            return Err(declaration_error(format!(
                "vocabulary '{vocabulary}': assigning a code to '{key}' would need {next}, past \
                 `{}`'s maximum of {max}. Never widen and never wrap — the remedy is a rebuild at \
                 a wider declared width (per-point-attributes §3.6)",
                width.arrow_type_name()
            )));
        }
        declared.codes.insert(key.clone(), next);
        taken.insert(next);
    }
    Ok(())
}

/// The rules a compiled code set must satisfy, whatever spelling it arrived in.
fn check_codes(
    codes: &BTreeMap<String, u32>,
    reserved: &[u32],
    width: ScalarType,
    vocabulary: &str,
) -> Result<()> {
    let max = width
        .max_code()
        .expect("a category width always has a maximum code");
    let mut seen: HashMap<u32, &str> = HashMap::new();
    let reserved_set: HashSet<u32> = reserved.iter().copied().collect();
    for (key, &code) in codes {
        if code == ABSENT_CODE {
            return Err(declaration_error(format!(
                "vocabulary '{vocabulary}': value '{key}' is assigned code 0, which is the \
                 reserved *absent* sentinel (per-point-attributes §3.6). Code 0 is what preserves \
                 `columns.arrow`'s contractual non-nullability without a validity buffer, so \
                 '{key} = 0' would make every value-less row a member of '{key}'"
            )));
        }
        if code > max {
            return Err(declaration_error(format!(
                "vocabulary '{vocabulary}': value '{key}' has code {code}, past `{}`'s maximum of \
                 {max}. Never widen and never wrap — the remedy is a rebuild at a wider declared \
                 width (per-point-attributes §3.6)",
                width.arrow_type_name()
            )));
        }
        if reserved_set.contains(&code) {
            return Err(declaration_error(format!(
                "vocabulary '{vocabulary}': value '{key}' has code {code}, which is also listed \
                 `reserved`. A retired code is never reassigned: reusing one silently recolours \
                 every row that carried it (per-point-attributes §3.4)"
            )));
        }
        if let Some(other) = seen.insert(code, key) {
            return Err(declaration_error(format!(
                "vocabulary '{vocabulary}': values '{other}' and '{key}' share code {code}. The \
                 row stores only the code, so two keys at one code are one colour under two names \
                 and no way back"
            )));
        }
    }
    Ok(())
}

// ---------------------------------------------------------------------------------------------
// Attributes
// ---------------------------------------------------------------------------------------------

fn compile_attributes(
    blocks: &[AttributeBlock],
    vocabularies: &HashMap<String, Vocabulary>,
) -> Result<Vec<Attribute>> {
    let mut attributes = Vec::with_capacity(blocks.len());
    let mut seen_names: HashSet<&str> = HashSet::new();

    for decl in blocks {
        if !seen_names.insert(decl.name.as_str()) {
            return Err(declaration_error(format!(
                "attribute '{}' is declared twice. The scalar tail is stored positionally, so two \
                 columns of one name is not a last-one-wins config question — it is two columns \
                 whose values are read back under one name",
                decl.name
            )));
        }
        check_column_name(&decl.name)?;
        // **Column names and filter combinators share one namespace** (decision 0062). A leaf in a
        // filter expression is a column name directly — there is no wrapper object — so a column
        // called `any_of` would be ambiguous with the combinator at request time. Refused at the
        // build instead, where it is one error against one declaration rather than a request that
        // means two things.
        if matches!(decl.name.as_str(), "all_of" | "any_of" | "none_of") {
            return Err(declaration_error(format!(
                "attribute '{}': that name is a filter combinator (decision 0062), and a filter \
                 expression names columns directly, so a column may not take one. Reserved: \
                 all_of, any_of, none_of",
                decl.name
            )));
        }
        // The attribute's own one-field map: `field` locates the column when it differs from the
        // served name, and the attribute pass reads it (`Attribute::field`). Empty is refused
        // rather than read as *the same as the name*: it names no column at all.
        if decl.field.as_deref().is_some_and(|f| f.trim().is_empty()) {
            return Err(declaration_error(format!(
                "attribute '{}': `field` is empty, so it names no column. Omit it to read the \
                 column named '{}'",
                decl.name, decl.name
            )));
        }
        // `render` + `multi` before bare `multi`: the first is a permanent fence (0039) and the
        // second an unbuilt stage, and a caller who set both must hear the fence — it survives the
        // epic that lifts the other refusal.
        if decl.render && decl.multi {
            return Err(declaration_error(format!(
                "attribute '{}': `render` with `multi = true` is never admissible (decision 0039) \
                 — a rendered mark has one colour, and no projection or summary of a list earns a \
                 hot column. Declare an ordinary single-valued attribute carrying the value to \
                 colour by",
                decl.name
            )));
        }
        if decl.multi {
            return Err(declaration_error(format!(
                "attribute '{}': `multi = true` is specified and not built (records §5 — the list \
                 addressing lands with the multi-value epic, records §13). Refused rather than \
                 read as single-valued: accepting it would store one value per item under a \
                 declaration promising several",
                decl.name
            )));
        }
        if decl.render_in.is_some() {
            return Err(declaration_error(format!(
                "attribute '{}': `render_in` is specified and not built (per-point-attributes \
                 §3.9). A per-view hot column needs contracts §2.6 to enumerate columns per view, \
                 which views §3 permits and the format does not yet carry — \
                 `MANIFEST.declared_scalars` is one flat bundle-wide list. Accepting it would put \
                 the column in every view anyway, silently, which is the opposite of what it asks \
                 for. Omit it: every view is the current behaviour and the documented default",
                decl.name
            )));
        }
        // Neither `render` nor `index` is not a refusal: the declaration is blob-resident (records
        // §3) — no hot-column slot, no entity-space structure, no `/v1/meta` operand; the record
        // blob holds its values and drill-down returns them.

        let ty_name = decl.ty.as_deref().ok_or_else(|| {
            declaration_error(format!(
                "attribute '{}': `type` is required and has no default (configuration.md §6). The \
                 declarable types are bool, u8, u16, u32, u64, i8, i16, i32, i64, f32, f64, \
                 timestamp_us, keyword, text and category",
                decl.name
            ))
        })?;

        let attribute = match ty_name {
            "category" => {
                let name = decl.vocabulary.as_deref().ok_or_else(|| {
                    declaration_error(format!(
                        "attribute '{}': `vocabulary` is required for a category and has no \
                         default (configuration.md §6). It names a `[[vocabulary]]` block, which \
                         is where the width, the value set and the visibility live — every one of \
                         them a decision nobody can make on the author's behalf",
                        decl.name
                    ))
                })?;
                // **Refused here, before a data file is opened**, and never an implicitly minted
                // open vocabulary: the fall-through §7 forbids, arriving through a typo. An
                // implicit vocabulary would take whatever width, value set and visibility the
                // fall-through picked, none of which anyone declared.
                let vocabulary = vocabularies.get(name).ok_or_else(|| {
                    declaration_error(format!(
                        "attribute '{}': `vocabulary = \"{name}\"` names no `[[vocabulary]]` \
                         block. Declared: {}. A missing block is refused rather than minted as an \
                         open vocabulary — a typo would otherwise create a value set nobody \
                         authored, at whatever width and visibility the fall-through picked",
                        decl.name,
                        declared_names(vocabularies)
                    ))
                })?;
                if let Some(analyser) = &decl.analyser {
                    return Err(declaration_error(format!(
                        "attribute '{}' is a category, not `text`, so `analyser = \"{analyser}\"` \
                         has no meaning for it. Refused rather than ignored: an ignored analyser is \
                         a pipeline its author believes is in use",
                        decl.name
                    )));
                }
                Attribute {
                    name: decl.name.clone(),
                    title: decl.title.clone(),
                    field: decl.field.clone(),
                    ty: vocabulary.width,
                    analyser: None,
                    vocabulary: Some(vocabulary.name.clone()),
                    value_set: Some(vocabulary.value_set),
                    // Every flag combination is legal for a category — a rendered category stays
                    // filterable because its entity-space structures are the constant floor, not a
                    // placement (records §4.2).
                    index: decl.index,
                    render: decl.render,
                }
            }
            // **`utf8` is retired as a declared type, and the refusal names its two successors**
            // (records §4.3, §4.4; decision 0048 makes this a refusal rather than an alias, because
            // a silent rename would give a schema a storage layout its author did not choose). It
            // remains the *wire* type of a keyword's value and of a category's key.
            "utf8" => {
                return Err(declaration_error(format!(
                    "attribute '{}': `utf8` is retired as a declared type. A short string matched \
                     whole — an identifier, an order number, a hostname — is `keyword`, which \
                     stores a per-layer sorted dictionary and a `u32` ordinal and keeps `eq`, \
                     `in`, `prefix` and `contains` byte-exact. Prose searched by word is `text` \
                     (records-and-search §4.4), whose values live in the record blob and whose \
                     terms come from a named analyser",
                    decl.name
                )));
            }
            other => {
                // A plain scalar: the type *is* the width, and none of the vocabulary machinery
                // applies. Refused rather than ignored if any of it is present.
                let ty = ScalarType::parse(other).ok_or_else(|| {
                    declaration_error(format!(
                        "attribute '{}': unknown type '{other}'. Declarable types are bool, u8, \
                         u16, u32, u64, i8, i16, i32, i64, f32, f64, timestamp_us, keyword, text \
                         and category",
                        decl.name
                    ))
                })?;
                if decl.vocabulary.is_some() {
                    return Err(declaration_error(format!(
                        "attribute '{}' is type '{other}', not a category, so `vocabulary` has no \
                         meaning for it. Refused rather than ignored: a value set on a column that \
                         has none is a disclosure control its author believes is set",
                        decl.name
                    )));
                }
                // **The analyser is resolved here** (decision 0070): a `text` column's terms are
                // whatever its named analyser produces, so a name this binary does not carry must
                // be refused at the declaration rather than defaulted — indexing a column with a
                // pipeline its author did not ask for is the silent mismatch the named shape
                // exists to prevent.
                let analyser = match (ty, decl.analyser.as_deref()) {
                    (ScalarType::Text, name) => {
                        let name = name.unwrap_or(tessera_analyse::UNICODE);
                        let resolved = tessera_analyse::analyser(name).ok_or_else(|| {
                            declaration_error(format!(
                                "attribute '{}': '{name}' is not an analyser this build carries. \
                                 Available: {}",
                                decl.name,
                                tessera_analyse::ANALYSER_NAMES.join(", ")
                            ))
                        })?;
                        Some(resolved.identity())
                    }
                    (_, Some(name)) => {
                        return Err(declaration_error(format!(
                            "attribute '{}' is type '{other}', not `text`, so `analyser = \
                             \"{name}\"` has no meaning for it. Refused rather than ignored: an \
                             ignored analyser is a pipeline its author believes is in use",
                            decl.name
                        )));
                    }
                    (_, None) => None,
                };
                // **`render` on `text` is refused for the reason `keyword`'s is, and one more.**
                // Prose is not a fixed-width slot, and a text column's value does not live in
                // entity space at all — it lives in the record blob, which no scan reads.
                if ty == ScalarType::Text && decl.render {
                    return Err(declaration_error(format!(
                        "attribute '{}': `render` on `text` is refused — the hot column is a \
                         fixed-width slot in every row and prose is not one, and a text column's \
                         value lives in the record blob, which no scan reads (records-and-search \
                         §3, §4.4). `index = true` gives it a token index and costs the hot column \
                         nothing",
                        decl.name
                    )));
                }
                if ty == ScalarType::Keyword && decl.render {
                    return Err(declaration_error(format!(
                        "attribute '{}': `render` on `keyword` is refused (configuration.md §6 — \
                         the hot column is a fixed-width slot in every row, and a keyword's value \
                         is not one). Its ordinal is fixed-width but is a per-layer index internal \
                         that never leaves the server (records §4.3). Declare a category, whose \
                         row cost is its width. `index = true` is available and costs the hot \
                         column nothing",
                        decl.name
                    )));
                }
                Attribute {
                    name: decl.name.clone(),
                    title: decl.title.clone(),
                    field: decl.field.clone(),
                    ty,
                    analyser,
                    vocabulary: None,
                    value_set: None,
                    index: decl.index,
                    render: decl.render,
                }
            }
        };
        attributes.push(attribute);
    }
    Ok(attributes)
}

fn declared_names(vocabularies: &HashMap<String, Vocabulary>) -> String {
    if vocabularies.is_empty() {
        return "none".to_string();
    }
    let mut names: Vec<&str> = vocabularies.keys().map(String::as_str).collect();
    names.sort_unstable();
    names.join(", ")
}

/// A column name that can be written into `columns.arrow`'s schema without colliding with the
/// fixed columns or with the ingest batch's reserved names.
///
/// The reserved set is transcribed rather than imported: `tessera-server`'s `RESERVED_COLUMNS`
/// belongs to a crate this one must not depend on, and the two are checked against each other in
/// this module's tests instead.
fn check_column_name(name: &str) -> Result<()> {
    const FIXED: [&str; 2] = ["tessera_id", "residual"];
    const INGEST_RESERVED: [&str; 5] = ["external_id", "x", "y", "access", "node_id"];
    if name.is_empty() {
        return Err(declaration_error("an attribute with an empty name"));
    }
    // The name is the column's **identifier**, not merely its display label: it addresses the
    // column in `/v1/categories/{column}` (contracts §3.2), and the duplicate check above is what
    // makes it unique bundle-wide. A path segment is therefore what it has to survive, so the
    // character set is closed here rather than escaped at every use site — one refusal at build
    // beats a percent-encoding convention that two readers can spell differently.
    if !name
        .bytes()
        .all(|b| b.is_ascii_alphanumeric() || b == b'_' || b == b'-')
    {
        return Err(declaration_error(format!(
            "attribute '{name}': a column name is its identifier on the wire \
             (`/v1/categories/{{column}}`, contracts §3.2), so it is limited to ASCII letters, \
             digits, `_` and `-`"
        )));
    }
    if FIXED.contains(&name) {
        return Err(declaration_error(format!(
            "attribute '{name}' shadows a fixed column of `columns.arrow` (contracts §2.6). The \
             reader refuses such a segment at load"
        )));
    }
    if name == "record" {
        return Err(declaration_error(
            "attribute 'record': the name is reserved — `attrs/record/` is the record blob's \
             namespace (records §2, review N10), so a column of that name would address the \
             blob's files as its own",
        ));
    }
    if INGEST_RESERVED.contains(&name) {
        return Err(declaration_error(format!(
            "attribute '{name}' shadows a reserved `/control/ingest` column (contracts §3.4), so \
             no batch could ever carry a value for it — the handler would read the reserved \
             column's meaning instead"
        )));
    }
    Ok(())
}

// ---------------------------------------------------------------------------------------------
// Layers
// ---------------------------------------------------------------------------------------------

/// What [`compile_layers`] hands back: the declarations, each layer's bound sources beside them,
/// and which layers the `[layer.labels]` sugar wrote, by parent — three parallel views of one pass,
/// kept apart because a [`LayerDeclaration`] is exactly the control-plane payload and must carry
/// neither of the others.
type CompiledLayers = (
    Vec<LayerDeclaration>,
    Vec<LayerSources>,
    BTreeMap<String, String>,
);

fn compile_layers(
    declared: &[LayerBlock],
    views: &[View],
    attributes: &[Attribute],
    sources: &Sources,
) -> Result<CompiledLayers> {
    // Sugar first, so nothing below this line knows a label layer from a layer.
    let (blocks, from_labels) = expand_labels(declared)?;
    let mut layers = Vec::with_capacity(blocks.len());
    let mut per_layer = Vec::with_capacity(blocks.len());
    let mut seen: HashSet<&str> = HashSet::new();
    for block in &blocks {
        if !seen.insert(block.name.as_str()) {
            return Err(declaration_error(format!(
                "layer '{}' is declared twice. A layer name is tombstoned on drop and never \
                 reused, since bookmarks, edges and suppressions all travel by it",
                block.name
            )));
        }
        let object = format!("layer '{}'", block.name);

        // **Refused here, before the artifacts file is opened**: a layer appears only in the views
        // it declares, so a mistyped view name would produce a bundle whose layer is registered,
        // reachable, and serves nothing — indistinguishable, from every client, from a layer whose
        // artifacts all failed their existence criterion.
        let declared_views = block.views.as_ref().ok_or_else(|| {
            declaration_error(format!(
                "layer '{}': `views` is required — it names the coordinate systems this layer's \
                 artifacts are drawn on, and a layer in no view is registered, reachable and empty",
                block.name
            ))
        })?;
        for view in declared_views {
            if !views.iter().any(|v| &v.name == view) {
                return Err(declaration_error(format!(
                    "layer '{}' declares view '{view}', which no `[[view]]` block declares. \
                     Declared: {}. A layer in a view that does not exist is registered, reachable \
                     and empty, which no client can tell from one whose artifacts were all withheld",
                    block.name,
                    if views.is_empty() {
                        "none".to_string()
                    } else {
                        views
                            .iter()
                            .map(|v| v.name.as_str())
                            .collect::<Vec<_>>()
                            .join(", ")
                    }
                )));
            }
        }

        let membership = compile_membership(block, attributes)?;

        let hierarchy = compile_hierarchy(block)?;
        let visibility = match block.visibility.as_deref() {
            Some(PUBLIC) => None,
            Some(label) => {
                check_label(&block.name, "visibility", label)?;
                Some(label.to_string())
            }
            None => {
                return Err(declaration_error(format!(
                    "layer '{}': `visibility` is required and has no default \
                     (configuration.md §6). Write the access label a viewer must hold to know this \
                     layer exists at all, or `public` to say every principal reaches it. There is \
                     no default because the value an absent line would supply is `public` — the \
                     widest one there is",
                    block.name
                )));
            }
        };

        let artifact = block.artifact_visibility.as_ref().ok_or_else(|| {
            declaration_error(format!(
                "layer '{}': `artifact_visibility` is required and has no default \
                 (configuration.md §6, C27). Write \
                 `artifact_visibility = {{ field = \"<column>\", default = \"<label>\" }}`: naming \
                 a `field` is the declaration that each artifact carries its own access label, and \
                 `default` is what one carrying none gets — an access label, or `inherited` to say \
                 the layer's own gate is the whole of it. Omit `field` for a layer whose artifacts \
                 are gated on their members' visibility instead. Mis-declared as carrying its own \
                 labels, a corpus-derived layer serves the existence and count of every artifact \
                 to every principal who reaches it",
                block.name
            ))
        })?;
        if let Some(field) = &artifact.field {
            if field.trim().is_empty() {
                return Err(declaration_error(format!(
                    "layer '{}': `artifact_visibility.field` is empty. Omit it to say artifacts \
                     carry no labels of their own",
                    block.name
                )));
            }
        }
        let default = artifact.default.as_deref().ok_or_else(|| {
            declaration_error(format!(
                "layer '{}': `artifact_visibility.default` is required and has no default. It is \
                 what an artifact carrying no label of its own gets: an access label, `public` for \
                 the one every principal holds, or `inherited` to say the layer's own gate is the \
                 whole of it",
                block.name
            ))
        })?;
        let artifact_visibility = ArtifactVisibility {
            field: artifact.field.clone(),
            default: if default == INHERITED {
                MemberDefault::Inherited
            } else {
                check_label(&block.name, "artifact_visibility.default", default)?;
                MemberDefault::Label(default.to_string())
            },
        };

        let require_member_visibility = compile_criterion(
            block.require_member_visibility.as_ref(),
            &block.name,
            "layer",
        )?;

        // ⊘ Parsed so the refusal can name what is absent, rather than left to
        // `deny_unknown_fields` — the key is real, it is `false` today, and a caller who writes
        // `true` must hear that the fold has no path for it instead of that the key does not exist.
        if block.withdraw_on_member_deletion == Some(true) {
            return Err(declaration_error(format!(
                "layer '{}': `withdraw_on_member_deletion = true` on a layer is specified and not \
                 built (annotation-write-cycle.md §6.1). It would drop the whole artifact when one \
                 of its members is deleted, and the fold has no artifact-withdrawal path — so \
                 accepting it would leave the artifact standing under a declaration saying it had \
                 gone. `false` is the default and the current behaviour: the membership shrinks and \
                 every computed property is recomputed from what is left. The **content**-level key \
                 of the same name, on `[layer.content]`, is built and defaults `true`",
                block.name
            )));
        }

        let content = compile_content(block)?;

        // ---- acquisition ---------------------------------------------------------------------
        // Read after the declaration is compiled, because the field map is checked *against* it:
        // `hierarchy` is what says there are parent edges, `depends_on` that there are attachment
        // edges, `content.supplied` that there is content, and `membership` that there are members
        // — so each of those keys decides whether the map may name the field that carries it.
        // **A layer names its own file or writes its artifacts out, never both.** They are two
        // spellings of one thing, so a layer declaring each leaves two answers to what its
        // artifacts are — and which one won would be this function's iteration order rather than
        // anything the caller wrote.
        if block.source.is_some() && block.artifacts.is_some() {
            return Err(declaration_error(format!(
                "{object}: `source` and an inline `artifacts` list are both declared. They are two \
                 spellings of one thing — a file this layer reads, or the artifacts written out in \
                 this document — so declaring both is a parse error rather than a precedence \
                 question"
            )));
        }
        // **No `[defaults].source` here either**: a layer with no source is declared and empty,
        // which is the normal state for a deployment that writes its artifacts through the service.
        let source = match &block.source {
            Some(declared) => Some(sources.path(&object, declared)?),
            None => None,
        };
        // **An inline row is already the canonical spelling**, so there is nothing for a map to
        // move: `fields` locates a column in a file, and a layer written out here names no file.
        if block.artifacts.is_some() && block.fields.is_some() {
            return Err(declaration_error(format!(
                "{object}: `fields` beside an inline `artifacts` list. The map locates this \
                 layer's fields in the file its `source` names, and an inline artifact carries the \
                 canonical names already — so there is no file for the names to be read out of"
            )));
        }
        let members_block = block.members.as_ref();
        let named = |field: &str| block.fields.as_ref().is_some_and(|f| f.contains_key(field));
        // **A membership is included or excluded, never both.** The two are one field written two
        // ways — the entities in the set, or the entities out of it — so a row carrying each would
        // have two memberships, and every masked count and every criterion divides by one of them.
        if named("members") && named("excluding") {
            return Err(declaration_error(format!(
                "{object}: `fields` names both `members` and `excluding`. They are two spellings of \
                 one field — the entities in the membership, or the entities it leaves out — and \
                 the build complements the second into the first, so naming both leaves two \
                 memberships for one artifact"
            )));
        }
        let inline_carries_members = block.artifacts.as_ref().is_some_and(|rows| {
            rows.iter()
                .any(|a| a.members.is_some() || a.excluding.is_some())
        });
        let carries_members = named("members") || named("excluding") || inline_carries_members;
        // **A layer's membership has two shapes, and it names whichever it uses** (§7). A list
        // field on the artifact row, or a source of its own, one row per `(artifact, entity)` —
        // for a membership no single cell should hold. Declaring both leaves two answers to what
        // an artifact's members are, and every masked count and every criterion divides by one of
        // them.
        if carries_members && members_block.is_some() {
            return Err(declaration_error(format!(
                "{object}: membership is declared twice — a `members` (or `excluding`) field on \
                 the artifact row, and a `[layer.members]` source of its own. They are two shapes \
                 of one thing, so declaring both is a parse error rather than a precedence \
                 question"
            )));
        }
        let enumerated = membership == MembershipSource::Enumerated;
        // **`value_set` decides whether a member key may create an artifact**
        // (`artifacts-from-points.md` §3). `closed` is the default and is the roster rule: the
        // artifacts source says which artifacts exist, and a key not on it is refused. `open` makes
        // that source enrichment instead — a cluster exists because points say it does.
        let value_set = match block.value_set.as_deref() {
            None | Some("closed") => ValueSet::Closed,
            Some("open") => ValueSet::Open,
            Some(other) => {
                return Err(declaration_error(format!(
                    "{object}: `value_set = \"{other}\"` is neither \"closed\" nor \"open\". \
                     Closed is the default: the layer's artifacts are the roster, and a member key \
                     not on it is refused. Open makes an unknown key create an artifact carrying \
                     nothing but its name"
                )));
            }
        };
        for artifact in block.artifacts.iter().flatten() {
            if artifact.members.is_some() && artifact.excluding.is_some() {
                return Err(declaration_error(format!(
                    "{object}: artifact '{}' declares both `members` and `excluding`. They are two \
                     spellings of one membership — the entities in it, or the entities it leaves \
                     out — so declaring both leaves two memberships for one artifact",
                    artifact.key
                )));
            }
            if !enumerated && (artifact.members.is_some() || artifact.excluding.is_some()) {
                return Err(declaration_error(format!(
                    "{object}: artifact '{}' carries a stored membership and this layer's \
                     `membership` is not `enumerated` — its members are computed from a shape or a \
                     predicate, so a stored set here is one nothing would read",
                    artifact.key
                )));
            }
        }
        let artifact_fields = check_fields(
            &object,
            source.as_ref(),
            &[
                KnownField::always("key"),
                KnownField::asserted_by(
                    "contents",
                    !content.supplied.is_empty(),
                    "`[[layer.content.supplied]]` is what declares this layer's artifacts carry \
                     content",
                ),
                KnownField::asserted_by(
                    "parent",
                    matches!(
                        hierarchy.kind,
                        HierarchyKind::Nested | HierarchyKind::Tiered
                    ),
                    "`hierarchy.kind` is `flat` or `stacked`, neither of which has lineage in its \
                     edges",
                ),
                KnownField::asserted_by(
                    "attached_layer",
                    !block.depends_on.is_empty(),
                    "`depends_on` is what names the layers this one's edges point into",
                ),
                KnownField::asserted_by(
                    "attached_key",
                    !block.depends_on.is_empty(),
                    "`depends_on` is what names the layers this one's edges point into",
                ),
                KnownField::asserted_by(
                    "members",
                    enumerated,
                    "`membership` is not `enumerated`, so this layer's members are computed \
                     rather than stored per artifact",
                ),
                KnownField::asserted_by(
                    "excluding",
                    enumerated,
                    "`membership` is not `enumerated`, so this layer's members are computed \
                     rather than stored per artifact",
                ),
            ],
            block.fields.as_ref(),
            ENTITY_ID,
        )?;

        let members = match members_block {
            None => None,
            Some(members) => {
                let object = format!("{object} `[layer.members]`");
                if !enumerated {
                    return Err(declaration_error(format!(
                        "{object}: a member source names one row per (artifact, entity), and this \
                         layer's `membership` is not `enumerated` — its members are computed from a \
                         shape or a predicate, so there is no stored set for the file to carry"
                    )));
                }
                let path = match &members.source {
                    Some(declared) => Some(sources.path(&object, declared)?),
                    None => None,
                };
                // **The artifacts are the roster** (`layers`): a member row names an artifact, and
                // without them there is nothing for the name to resolve against — a mistyped key
                // would publish a phantom artifact rather than fail. Under `value_set = "open"`
                // that is exactly what the caller asked for, so the refusal is the closed set's
                // alone (`artifacts-from-points.md` §3): a bare clustering declares no artifacts
                // and its clusters exist because its points name them.
                if value_set == ValueSet::Closed
                    && path.is_some()
                    && source.is_none()
                    && block.artifacts.is_none()
                {
                    return Err(declaration_error(format!(
                        "{object}: a member source without the layer's own artifacts. The layer's \
                         `source` — or its inline `artifacts` list — is the roster a member row \
                         names, so members with no roster would make every key its own artifact \
                         rather than a refusal. Declare `value_set = \"open\"` on the layer to \
                         have exactly that: an artifact per key the points name"
                    )));
                }
                let fields = check_fields(
                    &object,
                    path.as_ref(),
                    &[
                        KnownField::always("key"),
                        KnownField::always("entity"),
                        KnownField::asserted_by(
                            "rank",
                            !content.supplied.is_empty(),
                            "a rank names the generating set of `contents[k]`, and \
                             `[[layer.content.supplied]]` is what declares there is content",
                        ),
                    ],
                    members.fields.as_ref(),
                    ENTITY_ID,
                )?;
                path.map(|path| MemberSource { path, fields })
            }
        };
        per_layer.push(LayerSources {
            name: block.name.clone(),
            artifacts: match (source, block.artifacts.clone()) {
                (Some(path), _) => Some(ArtifactSource::File {
                    path,
                    fields: artifact_fields,
                }),
                (None, Some(rows)) => Some(ArtifactSource::Inline(rows)),
                // Legal, and the object declared and empty (`configuration.md` §2): a layer with
                // no artifacts yet is the normal state for a deployment that writes through the
                // service, and the empty bundle is what carries its schema.
                (None, None) => None,
            },
            members,
        });

        // **The layout pin, refused rather than ignored where the word is not one of the three.**
        // An ignored pin is the silent case: the operator declared a layout, got another, and has
        // nothing to look at (selection memo §4.1). The one *combination* refused here rather than
        // in `validate` is a shape with a row-major pin, and that one is `validate`'s — see
        // `DeclarationError::LayoutWithoutRowSource`.
        let layout = match block.layout.as_deref() {
            None => None,
            Some(word) => Some(ServingLayout::parse_pin(word).ok_or_else(|| {
                declaration_error(format!(
                    "layer '{}': `layout = \"{word}\"` is not a layout. The words are {} — \
                     `rows` is one row-space bitmap per artifact, `column` one artifact label per \
                     row for a level whose memberships partition the corpus, and `list` a list of \
                     labels per row for one whose memberships overlap. Omitting the key is the \
                     normal state: the pick is then automatic and re-evaluated at every fold",
                    block.name,
                    ServingLayout::PIN_VOCABULARY.join(", ")
                ))
            })?),
        };

        let shape = compile_shape(block, &membership)?;
        let declaration = LayerDeclaration {
            name: block.name.clone(),
            title: block.title.clone(),
            views: declared_views.clone(),
            membership,
            value_set,
            visibility,
            artifact_visibility,
            require_member_visibility,
            hierarchy,
            content,
            depends_on: block.depends_on.clone(),
            levels: compile_levels(block)?,
            layout,
            shape,
        };
        // **One implementation of the rules, not two.** Everything `LayerRegistry::prepare_create`
        // would refuse is refused here too, by calling the same check — so a declaration refused
        // online is refused here with the same words, at parse, before a data file is opened.
        declaration
            .validate()
            .map_err(|e| declaration_error(format!("layer '{}': {e}", declaration.name)))?;
        layers.push(declaration);
    }
    Ok((layers, per_layer, from_labels))
}

/// `membership` — where a layer's artifacts get their members, at its three spellings.
///
/// **Two words and a table**, and the table is not decoration: an attribute membership is a
/// predicate over a value column, and *which* column is part of the declaration. Spelled as a
/// bare word it would be a membership rule with nothing to evaluate, so the field rides the value
/// that asserts there is one — the same shape `point_visibility = { field }` takes, and the same
/// reason. A **spatial** membership carries its shape and depth in [`compile_shape`]'s own block
/// rather than here, because those are per-artifact facts and this field is not.
fn compile_membership(block: &LayerBlock, attributes: &[Attribute]) -> Result<MembershipSource> {
    let spellings = "\n  \
         membership = \"enumerated\"              # a stored set per artifact\n  \
         membership = \"spatial\"                 # a shape per artifact, from [layer.shape]\n  \
         membership = { attribute = \"severity\" }  # a predicate over that value column";
    let Some(value) = &block.membership else {
        return Err(declaration_error(format!(
            "layer '{}': `membership` is required — it decides what a write invalidates. \
             \"enumerated\" is a stored set per artifact, stale between the write and the \
             refresh; \"spatial\" is a shape decomposed at request time and never stale; \
             `{{ attribute = \"<field>\" }}` is a predicate over the value column it names, \
             likewise:{spellings}",
            block.name
        )));
    };
    match value {
        toml::Value::String(word) => match word.as_str() {
            "enumerated" => Ok(MembershipSource::Enumerated),
            "spatial" => Ok(MembershipSource::Spatial),
            // Named apart from the general refusal because it is the one wrong word a caller has
            // every reason to write: it *was* the spelling, and it is still the name of the thing.
            // Telling them the word does not exist would leave them looking for a fourth kind.
            "attribute" => Err(declaration_error(format!(
                "layer '{}': `membership = \"attribute\"` names no column. An attribute \
                 membership is a predicate over one value column, and which column is part of \
                 the declaration — write `membership = {{ attribute = \"<field>\" }}`:{spellings}",
                block.name
            ))),
            other => Err(declaration_error(format!(
                "layer '{}': `membership = \"{other}\"` is neither \"enumerated\" (a stored set \
                 per artifact) nor \"spatial\" (a shape, decomposed at request time):{spellings}",
                block.name
            ))),
        },
        toml::Value::Table(table) => {
            let mut keys: Vec<&str> = table.keys().map(String::as_str).collect();
            keys.sort_unstable();
            let ["attribute"] = keys.as_slice() else {
                return Err(declaration_error(format!(
                    "layer '{}': `membership` as a table takes exactly `attribute`, and this one \
                     carries {}:{spellings}",
                    block.name,
                    if keys.is_empty() {
                        "nothing".to_string()
                    } else {
                        keys.join(", ")
                    }
                )));
            };
            let field = table["attribute"].as_str().filter(|f| !f.trim().is_empty());
            let field = field.ok_or_else(|| {
                declaration_error(format!(
                    "layer '{}': `membership.attribute` must name the value column the predicate \
                     reads. An attribute membership with no column is a rule with nothing to \
                     evaluate:{spellings}",
                    block.name
                ))
            })?;
            // **Refused at the declaration, before a data file is opened** — the rule an attribute
            // naming an undeclared vocabulary already follows. A membership over a column nothing
            // declares is a predicate with nothing to read, and every artifact on that layer would
            // have an empty membership: served, counted at zero, and indistinguishable from a
            // layer whose artifacts were all withheld.
            if !attributes.iter().any(|a| a.name == field) {
                return Err(declaration_error(format!(
                    "layer '{}': `membership = {{ attribute = \"{field}\" }}` names no declared \
                     attribute. Declared: {}. A membership over a column nothing declares reads \
                     nothing, and every artifact on the layer would be published with an empty \
                     one",
                    block.name,
                    if attributes.is_empty() {
                        "none".to_string()
                    } else {
                        names(attributes.iter().map(|a| a.name.as_str()))
                    }
                )));
            }
            Ok(MembershipSource::Attribute(field.to_string()))
        }
        other => Err(declaration_error(format!(
            "layer '{}': `membership` is {other}, and it is one of two words or one \
             table:{spellings}",
            block.name
        ))),
    }
}

/// `[layer.shape]` — what a spatial layer's artifacts are shaped like, and how deep the tiles that
/// cover them are drawn.
///
/// **The depth is the membership rather than a tuning key** (ruling R3: the ranges *are* the
/// membership, and the polygon is content). A box covered by depth-4 tiles and the same box covered
/// at depth 8 hold different points, so there is no value for it to default to and none outside the
/// code space to accept.
///
/// ⊘ **`kind = "bbox"` is the whole vocabulary.** Each artifact carries `min_x`, `min_y`, `max_x`,
/// `max_y` on its own row; a polygon, a radius or a multi-part shape is refused here rather than
/// covered approximately, an approximate cover being a membership *wider* than the declaration.
///
/// A layer with no block at all is the state this surface has always had — declared for a shape it
/// does not yet carry, holding nothing, because publication into it is refused. That stays
/// expressible; what is refused is declaring artifacts for such a layer, which
/// [`compile_layers`] does where it can see both.
fn compile_shape(
    block: &LayerBlock,
    membership: &MembershipSource,
) -> Result<Option<ShapeDeclaration>> {
    let Some(declared) = &block.shape else {
        return Ok(None);
    };
    if *membership != MembershipSource::Spatial {
        return Err(declaration_error(format!(
            "layer '{}': `[layer.shape]` is declared and `membership` is not \"spatial\", so the \
             shape is a rule nothing evaluates — the members come from the stored set or the \
             predicate the membership names, and the box beside them would decide nothing",
            block.name
        )));
    }
    let kind = match declared.kind.as_deref() {
        None | Some("bbox") => ShapeKind::Bbox,
        Some(other) => {
            return Err(declaration_error(format!(
                "layer '{}': `shape.kind = \"{other}\"` is not \"bbox\", which is the only shape \
                 decoded. ⊘ A polygon or a radius is refused here rather than covered \
                 approximately: the tiles that cover a shape *are* its membership, so an \
                 approximate cover is a membership wider than the declaration",
                block.name
            )))
        }
    };
    let depth = declared
        .depth
        .filter(|d| *d > 0 && *d <= i64::from(tessera_types::layer::MAX_SHAPE_DEPTH))
        .ok_or_else(|| {
            declaration_error(format!(
                "layer '{}': `shape.depth` is {}, and it must be an integer between 1 and {}. It \
                 is the membership rather than a tuning key — a box covered by depth-`d` tiles \
                 holds different points at a different `d` — so there is no value for it to \
                 default to",
                block.name,
                match declared.depth {
                    Some(d) => d.to_string(),
                    None => "absent".to_string(),
                },
                tessera_types::layer::MAX_SHAPE_DEPTH
            ))
        })?;
    Ok(Some(ShapeDeclaration {
        kind,
        depth: depth as u8,
    }))
}

fn compile_hierarchy(block: &LayerBlock) -> Result<Hierarchy> {
    let declared = block.hierarchy.as_ref().ok_or_else(|| {
        declaration_error(format!(
            "layer '{}': `hierarchy` is required — the kind is declared and never inferred from \
             the edges. Write `hierarchy = {{ kind = \"flat\" }}` for one population of artifacts, \
             \"nested\" for a tree held in the edges, \"stacked\" for independent analyses one per \
             level, or \"tiered\" for containment edges running coarser → finer between levels. \
             `prune_children = true` serves only the deepest passing artifact per branch",
            block.name
        ))
    })?;
    let kind = match declared.kind.as_deref() {
        Some("flat") => HierarchyKind::Flat,
        Some("nested") => HierarchyKind::Nested,
        Some("stacked") => HierarchyKind::Stacked,
        Some("tiered") => HierarchyKind::Tiered,
        Some(other) => {
            return Err(declaration_error(format!(
                "layer '{}': `hierarchy.kind = \"{other}\"` is none of \"flat\", \"nested\", \
                 \"stacked\" or \"tiered\"",
                block.name
            )));
        }
        None => {
            return Err(declaration_error(format!(
                "layer '{}': `hierarchy.kind` is required. \"flat\" is one population with no \
                 lineage; \"nested\" is a tree held in the edges, every artifact at level 0; \
                 \"stacked\" is independent analyses, one per level; \"tiered\" is containment \
                 edges running coarser → finer between levels",
                block.name
            )));
        }
    };
    Ok(Hierarchy {
        kind,
        prune_children: declared.prune_children,
    })
}

fn compile_levels(block: &LayerBlock) -> Result<Vec<LevelDeclaration>> {
    let mut levels = Vec::with_capacity(block.levels.len());
    for entry in &block.levels {
        let level = entry.level.ok_or_else(|| {
            declaration_error(format!(
                "layer '{}': a level declares no `level` number. It is explicit rather than the \
                 array position, because edges reference `(layer, level, ordinal)` and reordering \
                 the file would silently renumber them",
                block.name
            ))
        })?;
        levels.push(LevelDeclaration {
            level,
            title: entry.title.clone(),
            zoom: entry.zoom,
        });
    }
    Ok(levels)
}

fn compile_content(block: &LayerBlock) -> Result<ContentDeclaration> {
    let Some(content) = &block.content else {
        return Ok(ContentDeclaration::default());
    };
    let mut supplied = Vec::with_capacity(content.supplied.len());
    for entry in &content.supplied {
        let ty = entry.ty.as_deref().ok_or_else(|| {
            declaration_error(format!(
                "layer '{}': supplied content '{}' declares no `type`. It is published on \
                 `/v1/meta` so a client knows what to draw: text, polygon, extent or point",
                block.name, entry.name
            ))
        })?;
        if !matches!(ty, "text" | "polygon" | "extent" | "point") {
            return Err(declaration_error(format!(
                "layer '{}': supplied content '{}' declares `type = \"{ty}\"`, which is none of \
                 text, polygon, extent or point. The set is closed because `/v1/meta` publishes it \
                 and a client draws from it",
                block.name, entry.name
            )));
        }
        let requirement = match entry.require_member_visibility.as_deref() {
            Some("all") => SuppliedRequirement::All,
            Some(INHERITED) => SuppliedRequirement::Inherited,
            Some(other) => {
                return Err(declaration_error(format!(
                    "layer '{}': supplied content '{}' declares \
                     `require_member_visibility = \"{other}\"`, and this slot takes only \"all\" or \
                     \"inherited\". Containment is all-or-nothing for content generated from \
                     documents, so there is no threshold between them",
                    block.name, entry.name
                )));
            }
            None => {
                return Err(declaration_error(format!(
                    "layer '{}': supplied content '{}' declares no `require_member_visibility`, \
                     and it has no default (configuration.md §6, C28). `\"all\"` is for content \
                     generated from documents — served only to a viewer who can see everything it \
                     was generated from, and it must arrive with a generating set. `\"inherited\"` \
                     is for content true whether or not a document exists, and it must **not** \
                     declare one: a set that is never tested is a claim the service would carry \
                     without meaning",
                    block.name, entry.name
                )));
            }
        };
        supplied.push(SuppliedContent {
            name: entry.name.clone(),
            ty: ty.to_string(),
            require_member_visibility: requirement,
        });
    }
    Ok(ContentDeclaration {
        computed: content.computed.clone(),
        supplied,
        // Defaulted `true` — the one direction a disclosure control may default in, the widening
        // half being the one that must be typed (C7).
        withdraw_on_member_deletion: content.withdraw_on_member_deletion.unwrap_or(true),
    })
}

/// `require_member_visibility` — the second axis, at its five settings.
///
/// **Five settings, two mechanisms.** `"any"` is `{ count = 1 }` and `"all"` is
/// `{ fraction = 1.0 }`, so the words are spellings of the thresholds rather than variants beside
/// them — which is why a layer declaring `"all"` inherits the proportional form's two properties:
/// it ⊘ breaks rollup, and it is refused on predicate membership, whose declared size is not a
/// number to divide by.
fn compile_criterion(
    value: Option<&toml::Value>,
    object: &str,
    what: &str,
) -> Result<Option<ExistenceCriterion>> {
    let Some(value) = value else {
        return Err(declaration_error(format!(
            "{what} '{object}': `require_member_visibility` is required and has no default \
             (configuration.md §6). It says how much of this object's membership the viewer must \
             already see: \"all\", \"any\", `{{ count = n }}`, `{{ fraction = p }}`, or \"none\" for \
             no such rule. There is no default because the value an absent line would supply is \
             \"none\", which serves the existence and count of every artifact down to a single \
             member"
        )));
    };
    match value {
        toml::Value::String(word) => match word.as_str() {
            "none" => Ok(None),
            "any" => Ok(Some(ExistenceCriterion::Count(1))),
            "all" => Ok(Some(ExistenceCriterion::Fraction(1.0))),
            other => Err(declaration_error(format!(
                "{what} '{object}': `require_member_visibility = \"{other}\"` is none of \"all\", \
                 \"any\" or \"none\". For a threshold write `{{ count = n }}` or \
                 `{{ fraction = p }}`"
            ))),
        },
        toml::Value::Table(table) => {
            let mut keys: Vec<&str> = table.keys().map(String::as_str).collect();
            keys.sort_unstable();
            match keys.as_slice() {
                ["count"] => {
                    // `>= 1`, not `>= 0`. A threshold every masked count clears is a requirement
                    // spelled as though it were one, and the caller who means *no requirement* has
                    // a word for it — the same reason an absent criterion is its own declaration
                    // rather than a permissive default (decision 0084).
                    let n = table["count"]
                        .as_integer()
                        .and_then(|n| u64::try_from(n).ok())
                        .filter(|n| *n >= 1);
                    n.map(|n| Some(ExistenceCriterion::Count(n)))
                        .ok_or_else(|| {
                            declaration_error(format!(
                                "{what} '{object}': `require_member_visibility.count` must be an \
                             integer of at least 1. A count of zero is cleared by every masked \
                             count, so it declares a requirement and imposes none; write \
                             `require_member_visibility = \"none\"` to say there is no rule"
                            ))
                        })
                }
                ["fraction"] => {
                    // The interval is half-open at zero for `count`'s reason, and closed at one
                    // because a share above the whole is unsatisfiable — a criterion no artifact
                    // can ever clear hides the layer rather than declaring anything.
                    let p = table["fraction"]
                        .as_float()
                        .filter(|p| *p > 0.0 && *p <= 1.0);
                    p.map(|p| Some(ExistenceCriterion::Fraction(p)))
                        .ok_or_else(|| {
                            declaration_error(format!(
                            "{what} '{object}': `require_member_visibility.fraction` must be a \
                             float in (0, 1]. Zero is cleared by every masked count and declares \
                             nothing — write `require_member_visibility = \"none\"` for that — \
                             and a share above one can never be cleared"
                        ))
                        })
                }
                _ => Err(declaration_error(format!(
                    "{what} '{object}': `require_member_visibility` as a table takes exactly one \
                     of `count` or `fraction`, and this one carries {}. `count` is the form under \
                     which rollup is guaranteed; `fraction` is the form that scales",
                    keys.join(", ")
                ))),
            }
        }
        other => Err(declaration_error(format!(
            "{what} '{object}': `require_member_visibility` is {other}, and it must be one of \
             \"all\", \"any\", `{{ count = n }}`, `{{ fraction = p }}` or \"none\""
        ))),
    }
}

#[cfg(test)]
mod tests;
