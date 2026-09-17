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
//! [`Extent::Auto`] and [`Extent::AutoLonLat`] are the values this module cannot resolve on its
//! own: [`frame_view`] reads the view's points source to fit the box, which is legitimate precisely
//! because the alternative is an operator guessing a frame their data has already decided.
//!
//! **The view's `projection` decides which spellings its `extent` and its `fields` take**
//! (`projections.md` §2). Under the default, `none`, both are what they have always been. Under a
//! projection the coordinate columns are `lon`/`lat` and the frame is a box in degrees, snapped
//! outward to the enclosing aligned square — the two sets do not overlap, and each refusal names
//! the set the view's own projection admits.
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
//! corpus with no permission model. `default` is optional (decision 0133): where a view declares
//! one, a point carrying no label takes it at the build and on `/control/ingest` alike; where it
//! declares none, both entry points refuse such a point naming the count. A view declaring neither
//! an acquisition key nor a default is refused at parse, having no label for any point.
//!
//! **Filling never overrides**, and that is inadmissible rather than merely unwise: a point's terms
//! are disjunctive — `M_auth` is a union of posting lists — so a label added to a point can only
//! widen it. A null value and an empty list both mean *no access terms*, which means visible to no
//! principal; neither means unrestricted, and where a default is declared those are the rows it
//! fills.

use std::collections::{BTreeMap, BTreeSet, HashMap, HashSet};
use std::path::{Path, PathBuf};

use serde::Deserialize;
use tessera_plugin::Plugin;
use tessera_spatial::frame::{snap_outward, Snap};
use tessera_spatial::tiler::ScalarType;
use tessera_spatial::{cell, Bounds, Projection};
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
    /// `[[view_group]]` — a set of views that share every setting and differ by a key
    /// (`views.md` §3.1). Beside `[[view]]` rather than inside it: a group is not a view, it
    /// cannot be named on a viewer verb, and its roster is a key set a plain view has no shape
    /// for.
    #[serde(default)]
    view_group: Vec<ViewGroupBlock>,
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
    /// The view whose Morton code breaks entity-id ties within a signature group
    /// ([decision 0112](../decisions/0112-the-anchor-view-orders-a-signature-groups-ids.md)).
    ///
    /// Required when the declaration carries more than one view, and **explicit rather than
    /// positional**: reordering declaration blocks must not silently re-key a rebuild, the ids
    /// being permanent (I9). A group name is not a view — the anchor is one coordinate system,
    /// so a group's view is named `<group>:<key>`.
    #[serde(default)]
    allocation_view: Option<String>,
}

/// `[[view]]` — one named coordinate system.
#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct ViewBlock {
    name: String,
    #[serde(default)]
    title: Option<String>,
    /// The function that turns a place on the Earth into a coordinate in this view's frame
    /// (`projections.md` §5), from the closed set and defaulting to `none`. Held as a string and
    /// resolved by [`Projection::from_name`] so a name outside the set is refused listing the
    /// ones inside it, rather than reported as *no variant matched*.
    #[serde(default)]
    projection: Option<String>,
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
    /// One label, or a list of labels (`views.md` §6, decision 0132).
    #[serde(default)]
    visibility: Option<tessera_types::view::DeclaredGate>,
}

/// `[[view_group]]` — a set of views sharing every setting, differing by a key and per-view
/// metadata (`views.md` §3.1, [decision 0108](../../../docs/decisions/0108-a-view-group-grows-by-its-roster.md)).
///
/// **Every `[[view]]` key, with the same meaning, plus the roster.** The roster is the whole of
/// what a group has and a view does not, and it decides where the points come from: under
/// `[[view_group.view]]` (form A) each view names its own file and the group names none, exactly
/// as a layer's file is the layer; under `[view_group.views]` (form B) the group's own `source`
/// holds every view's points with `fields.view` saying which view each row lands in. Declaring
/// both is refused, as `source` beside inline `artifacts` is; declaring neither mints the views
/// from the discriminator's distinct values and carries no metadata.
///
/// **`view` is held as `toml::Value` and not as a struct**, because a `[[view_group.view]]` block
/// mixes a closed key set with the group's declared metadata names — so no derive knows its field
/// list, and `deny_unknown_fields` cannot be the thing that closes it. `configuration.md` §1's
/// guarantee is kept by hand in [`compile_roster_view`], against the closed set plus the declared
/// names, which is the same manual route `extent`'s four spellings already take.
#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct ViewGroupBlock {
    name: String,
    #[serde(default)]
    title: Option<String>,
    #[serde(default)]
    projection: Option<String>,
    /// Form B's points file, one row per `(entity, view)`. **Form A declares none** — the file is
    /// the view — and `[defaults].source` deliberately does not reach here: a defaulted group
    /// source would turn a form A declaration into a form B one, or mint views from a
    /// discriminator column nobody named.
    #[serde(default)]
    source: Option<String>,
    #[serde(default)]
    fields: Option<BTreeMap<String, String>>,
    #[serde(default)]
    extent: Option<toml::Value>,
    #[serde(default)]
    point_visibility: Option<PointVisibilityBlock>,
    /// One label, or a list of labels (`views.md` §6, decision 0132).
    #[serde(default)]
    visibility: Option<tessera_types::view::DeclaredGate>,
    /// Another group's name: this group's views are that group's (`views.md` §3.3). Chains are
    /// refused, so the owner of a key set is always one hop away.
    #[serde(default)]
    members: Option<String>,
    /// The per-view values a view carries, `name = type` over the `[[attribute]]` types; a
    /// category is `{ type = "category", vocabulary = … }`.
    #[serde(default)]
    metadata: Option<BTreeMap<String, toml::Value>>,
    /// `[[view_group.view]]` — form A's roster, one block per view.
    #[serde(default)]
    view: Vec<toml::Value>,
    /// `[view_group.views]` — form B's roster, as a table.
    #[serde(default)]
    views: Option<RosterTableBlock>,
}

/// `[view_group.views]` — the roster as a table beside the group's own points file.
///
/// Two keys, and no more: what the table carries is fixed by the group's own declaration — the
/// canonical `key`, `visibility` and the declared metadata names — so `fields` locates them and
/// nothing here asserts one into existence (`configuration.md` §8).
#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct RosterTableBlock {
    #[serde(default)]
    source: Option<String>,
    #[serde(default)]
    fields: Option<BTreeMap<String, String>>,
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
    /// The projected view's spelling, and the only one it takes (`projections.md` §4.2).
    #[serde(default)]
    lon: Option<[f64; 2]>,
    #[serde(default)]
    lat: Option<[f64; 2]>,
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
    /// `"entity"` (the default) or `{ group = "<view_group>" }` — whether this column is one
    /// value per entity or one per `(entity, view of the group)` (`views.md` §5,
    /// [decision 0109](../../../docs/decisions/0109-scope-binds-an-attribute-or-layer-to-a-groups-views.md)).
    /// Held as a `toml::Value` because the two spellings are a word and a table, and a hand-written
    /// match names them rather than reporting *no variant matched* ([`compile_scope`]).
    #[serde(default)]
    scope: Option<toml::Value>,
    /// Where this column's own `source` spells the fields it is read by. **One key, `view`**, and
    /// only a group-scoped column with a `source` of its own has anything to name with it: that
    /// file carries one row per `(entity, view)`, and the discriminator says which view each row's
    /// value is for (`views.md` §5). Absent is the column `view`, the same default a scoped
    /// layer's `fields.view` takes. The entity id is `entity_id_field` beside it rather than a key
    /// here, which is the spelling every attribute already had.
    #[serde(default)]
    fields: Option<BTreeMap<String, String>>,
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
    /// `"entity"` (the default) or `{ group = "<view_group>" }` — one artifact set drawn on every
    /// view the layer names, or a different set per view of the group (`views.md` §3.5,
    /// decision 0109). The same key an attribute takes, with the same meaning.
    #[serde(default)]
    scope: Option<toml::Value>,
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
    /// `[layer.shape]` — what kind of shape a `membership = "spatial"` layer's artifacts carry
    /// ([`compile_shape`]).
    #[serde(default)]
    shape: Option<ShapeBlock>,
    /// The space the layer's artifact table writes its geometry in, where a row carries no `space`
    /// of its own (`polygon-membership.md` §4.3) — `"view"` if absent. On the layer beside
    /// `source` and `fields` because it is an acquisition-side fact about the file, on the same
    /// register those two are. Declarable on a layer carrying either kind of geometry: a
    /// membership shape, or an authored shape content, which is read in the same space (§6.1).
    #[serde(default)]
    default_space: Option<String>,
}

/// `[layer.shape]` as written. `kind` is optional *here* and not in the compiled form: absence is
/// what makes the message name the key rather than reporting *no variant matched*, which is the
/// same reason `membership` is held as a `toml::Value`. `depth` is held so that one written is
/// refused naming where it went (`polygon-membership.md` §6.1) rather than as an unknown key.
#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
struct ShapeBlock {
    #[serde(default)]
    kind: Option<String>,
    #[serde(default)]
    depth: Option<toml::Value>,
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
    /// The artifact's shape, in its layer's kind's field and no other (`polygon-membership.md`
    /// §6.1): `bbox = [min_x, min_y, max_x, max_y]`, `circle = [cx, cy, r]`,
    /// `ellipse = [cx, cy, a, b, angle]` or `wkt = "POLYGON ((…))"`. Refused on a layer whose
    /// membership is not `spatial`.
    #[serde(default)]
    pub bbox: Option<Vec<f64>>,
    #[serde(default)]
    pub circle: Option<Vec<f64>>,
    #[serde(default)]
    pub ellipse: Option<Vec<f64>>,
    #[serde(default)]
    pub wkt: Option<String>,
    /// The space the row's geometry is written in — `"view"` if absent, or `"wgs84"`, which a
    /// view declaring a projection honours by putting the coordinates through it
    /// (`polygon-membership.md` §4.3). It governs **every** geometry the row declares: the shape
    /// above, and the authored shape content in a `contents` cell, which is read in the same
    /// space as the same producer's membership polygon (§6.1).
    #[serde(default)]
    pub space: Option<String>,
    /// The parent artifacts in a hierarchy, by key — one under `nested` or `tiered`, and under
    /// `dag` as many as the artifact sits beneath (`dag-hierarchies.md` §4). Written as one string
    /// or as a list; a scalar is a list of one, exactly as the artifact table's `parent` column is
    /// read.
    #[serde(default, deserialize_with = "one_or_many")]
    pub parent: Vec<String>,
    #[serde(default)]
    pub attached_layer: Option<String>,
    #[serde(default)]
    pub attached_level: u32,
    #[serde(default)]
    pub attached_key: Option<String>,
}

/// A key written as one string or as a list of them — the two spellings of an artifact row's
/// `parent` cell, which under `dag` may name several (`dag-hierarchies.md` §4).
fn one_or_many<'de, D: serde::Deserializer<'de>>(
    deserializer: D,
) -> std::result::Result<Vec<String>, D::Error> {
    #[derive(Deserialize)]
    #[serde(untagged)]
    enum OneOrMany {
        One(String),
        Many(Vec<String>),
    }
    Ok(match OneOrMany::deserialize(deserializer)? {
        OneOrMany::One(one) => vec![one],
        OneOrMany::Many(many) => many,
    })
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
    /// The declared view groups, in declaration order (`views.md` §3).
    ///
    /// A build materialises every view of every one of them ([`Config::build_views`]). ⊘ What
    /// has no build behind it is the *running* half of spec §3.2 — the roster object and the
    /// create operation — so a group's views are the ones the declaration enumerates and no key
    /// comes into being after the build.
    pub view_groups: Vec<ViewGroup>,
    /// Which attributes and which layers are bound to a group's views (`views.md` §5, §3.5).
    pub scopes: Scopes,
    /// The group-scoped attributes, in declaration order — the column families a build writes
    /// beside the entity-space schema (`views.md` §5).
    pub scoped_attributes: Vec<ScopedAttribute>,
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
    /// `[defaults].allocation_view` as written, or `None` where the declaration named none.
    ///
    /// **The anchor is a view id, resolved against the registry [`Config::build_views`] builds**
    /// rather than against `[[view]]` alone: a group's view is nameable as `<group>:<key>` and is
    /// an ordinary candidate ([decision 0112](../decisions/0112-the-anchor-view-orders-a-signature-groups-ids.md)).
    pub allocation_view: Option<String>,
    /// Every declared attribute's name in declaration order, the group-scoped ones included.
    ///
    /// **Held here because the two halves are two lists.** A scoped column is not in
    /// [`Schema::attributes`] — it has no slot in the manifest's flat list — so neither list alone
    /// is the declaration's order, and the control-plane emitter states the order the author
    /// wrote ([`control_payloads`]).
    pub attribute_order: Vec<String>,
    /// Every declared vocabulary's name in declaration order. [`Schema::vocabularies`] is keyed by
    /// name and a map has no order to state.
    pub vocabulary_order: Vec<String>,
    /// Vocabulary name → the `[sources]` key its values are read from, for the vocabularies that
    /// name one.
    ///
    /// **Beside the declaration rather than inside it**, on [`Config::layer_sources`]' rule: a
    /// [`Vocabulary`] is what the manifest carries, and the file its keys came from is not part of
    /// the value set. It is kept because a sourced value set's keys are *rows* — the emitter says
    /// which source they are in rather than putting a corpus's contents in a declaration payload.
    pub vocabulary_sources: BTreeMap<String, String>,
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
    /// `default_space` is the space a row's shape is in where the row names none.
    File {
        path: PathBuf,
        fields: Fields,
        default_space: tessera_store::derived::ShapeSpace,
    },
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
    /// today, a **layer's** is, and a **view group's** is (contracts §3.2 r61).
    pub title: Option<String>,
    /// What turns this view's input coordinates into positions in its frame
    /// (`projections.md` §5). [`Projection::None`] — the default — transforms nothing, and the
    /// coordinates keep exactly the meaning they have in the file.
    pub projection: Projection,
    /// This view's geometry: `entity_id` with either `x`/`y` or `morton`/`residual`. `None` when
    /// the view declares no source, which is legal to *declare* and refused at a build that would
    /// have to read it.
    pub source: Option<PathBuf>,
    /// Where the identity and geometry fields sit in that file. Canonical is `entity_id` with
    /// either `x`/`y` or `morton`/`residual`.
    ///
    /// **A projected view's coordinate columns are `lon` and `lat`** (`projections.md` §2), and
    /// they resolve onto the canonical `x`/`y` here: what differs is the axis's *meaning* before
    /// the transform, and every reader below this point sees a coordinate pair either way.
    pub fields: Fields,
    /// The frame every position in this view is quantised across (`configuration.md` §1).
    /// The two `auto` spellings still need the data: [`frame_view`] turns them into [`Bounds`],
    /// and turns a [`Extent::LonLat`] box into the aligned square containing it.
    pub extent: Extent,
    /// The view's own gate: the labels a principal must hold one of, each element one term
    /// (`views.md` §6, decision 0132); `None` is `public`. Compiled by [`compile_view_gate`],
    /// which has asked the plugin to read every element.
    pub visibility: Option<Vec<String>>,
    /// Where each point's own access label is, and what a point carrying none gets.
    pub point_visibility: PointVisibility,
}

/// One declared view group: the settings its views share, and the roster that says which views
/// it has (`views.md` §3.1).
///
/// **A group is not a view.** It cannot be named on a viewer verb, has no row space and no
/// permutation; its views are views in every respect below the declaration, each addressed as
/// `<group>:<key>`. What is held here is the half of a view that is the same for all of them —
/// projection, extent, point visibility, gate — beside the roster that differs.
#[derive(Debug, Clone)]
pub struct ViewGroup {
    pub name: String,
    /// The group's human-readable title, published as `MANIFEST.groups[..].title` and served on
    /// `/v1/meta` as `groups[..].title` (contracts §3.2 r61). Absent is served as `null`.
    pub title: Option<String>,
    pub projection: Projection,
    /// Form B's points file, one row per `(entity, view)`, with [`ViewGroup::fields`]'s `view`
    /// naming the discriminator. `None` under form A, where each roster view names its own.
    pub source: Option<PathBuf>,
    /// Where the identity, geometry and discriminator fields sit. It locates the group's own
    /// source under form B, and each roster view's source under form A — the two carry the same
    /// per-point columns, the discriminator excepted, because they are two spellings of one thing.
    pub fields: Fields,
    /// The frame every position in every view of this group is quantised against. One frame for
    /// the group, which is what makes its views comparable and a key set meaningful.
    pub extent: Extent,
    pub point_visibility: PointVisibility,
    /// The group's own gate, the outer bound over every view of it (`views.md` §6): a list of
    /// labels, each one term; `None` is `public`.
    pub visibility: Option<Vec<String>>,
    /// The group whose views these are, where this group declares `members` (`views.md` §3.3);
    /// `None` where it owns them. Chains are refused, so this always names an owner.
    pub members: Option<String>,
    /// The per-view values a view of this group carries, in declaration order. Empty on a
    /// `members` group and on one whose views are minted from a discriminator.
    pub metadata: Vec<ViewMetadata>,
    pub roster: Roster,
}

/// One declared per-view metadata name and its type (`views.md` §3.1).
///
/// **View metadata is not an attribute** (`views.md` §5): it is one value per view rather than one
/// per `(entity, view)`, it lives on the roster, it filters nothing, and it is served typed on
/// `/v1/meta`. The two are kept apart here so that neither grows the other's surface.
#[derive(Debug, Clone)]
pub struct ViewMetadata {
    pub name: String,
    /// For a category this is the **vocabulary's** width, exactly as an attribute's is.
    pub ty: ScalarType,
    /// The vocabulary a category's keys are drawn from; `None` for a plain scalar.
    pub vocabulary: Option<String>,
}

impl ViewMetadata {
    /// The kind this name takes on the wire, as the manifest publishes it and as
    /// `PUT /control/view_groups/{name}` takes it.
    ///
    /// **One implementation** ([decision 0139](../../../docs/decisions/0139-one-implementation-between-build-and-ingest-and-across-a-type-family.md)):
    /// the manifest's group registry and the control-plane emitter answer the same declaration the
    /// same way, so a build and a runtime declaration of one block cannot disagree about a name's
    /// type — which is what the create operation's type check measures a supplied value against.
    pub fn declared_type(&self) -> tessera_store::manifest::ViewMetadataType {
        use tessera_store::manifest::ViewMetadataType;
        match (self.vocabulary.is_some(), self.ty) {
            (true, _) => ViewMetadataType::Category,
            (false, ScalarType::Bool) => ViewMetadataType::Bool,
            (false, ScalarType::F32) | (false, ScalarType::F64) => ViewMetadataType::Float,
            // `text` and `keyword` both hold a string; the served type is what a client renders,
            // and both render as text. The fallthrough to the integer arm they took before was
            // caught by the conformance work (2026-08-31): a build looked right because `/v1/meta`
            // types off the stored value, and what would have bitten is the create operation's
            // type check refusing a text value.
            (false, ScalarType::Utf8)
            | (false, ScalarType::Text)
            | (false, ScalarType::Keyword) => ViewMetadataType::Text,
            (false, ScalarType::TimestampUs) => ViewMetadataType::TimestampUs,
            (false, _) => ViewMetadataType::Int,
        }
    }
}

/// A metadata value as one roster record carries it, typed against its declaration.
#[derive(Debug, Clone, PartialEq)]
pub enum MetadataValue {
    Bool(bool),
    /// Every integer width, and a category's key resolved to its code.
    Int(i64),
    Float(f64),
    Text(String),
    /// Microseconds since the Unix epoch — the one time unit a `timestamp_us` may hold, so a
    /// declaration and a reader cannot disagree about it (`ScalarType::TimestampUs`).
    TimestampUs(i64),
}

/// Which views a group has, and therefore where its points come from (`views.md` §3.1).
///
/// **The roster decides the acquisition, not only the enumeration**, which is why the three arms
/// are one type rather than a roster beside a source: a group in form A has no source of its own
/// and a group in form B has no per-view source, so a shape admitting both would admit the
/// declaration that says the points are in two places.
#[derive(Debug, Clone)]
pub enum Roster {
    /// **Form A** — `[[view_group.view]]` blocks, one per view, each naming its own points file.
    Inline(Vec<RosterView>),
    /// **Form B** — `[view_group.views]`, the roster as a table beside the group's own points
    /// file, whose `fields.view` says which view each row lands in.
    Table(RosterTable),
    /// **Neither form** — the views are minted from the distinct values of the group's own
    /// discriminator, and carry no metadata and no gate of their own.
    Discriminator,
}

/// One view of a group, declared inline (`views.md` §3.1's form A).
#[derive(Debug, Clone)]
pub struct RosterView {
    /// The caller's own name for the view, required at creation and never reused
    /// (`views.md` §3.2). `<group>:<key>` is the view id.
    pub key: String,
    /// This view's points. `None` is legal and is a view declared and empty, exactly as it is on
    /// a plain `[[view]]`.
    pub source: Option<PathBuf>,
    /// This view's own gate, narrowing the group's, in the shape [`ViewGroup::visibility`]
    /// takes; `None` takes the group's.
    pub visibility: Option<Vec<String>>,
    /// This view's metadata, one entry per declared name.
    pub metadata: BTreeMap<String, MetadataValue>,
}

/// The roster as a table (`views.md` §3.1's form B): one row per view, carrying the canonical
/// `key`, `visibility` and the declared metadata names.
#[derive(Debug, Clone)]
pub struct RosterTable {
    pub source: PathBuf,
    pub fields: Fields,
}

/// What an attribute or a layer is bound to (`views.md` §5, §3.5; decision 0109).
///
/// **Entity scope is the default and declares nothing**, because there is nothing a declaration
/// could add: a constant attribute is one value per entity, evaluated in entity space, and
/// therefore visible under every view. The case that needs declaring is the value that differs by
/// view of a group.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Scope {
    Entity,
    /// The group whose views this object is per-view over. Always a group that owns its views: a
    /// scope naming a `members` group is refused pointing at the owner.
    Group(String),
}

/// One attribute whose values are per view of a group (`views.md` §5).
///
/// **A family, not a column**: one entity-space column per view of the group, each with its own
/// presence bitmap (decision 0064), written under `attrs/<column>/<group>/<key>/`. It is held
/// beside [`Schema::attributes`] rather than in it because the manifest's declared scalars are one
/// flat bundle-wide list, and a family has no slot there.
#[derive(Debug, Clone)]
pub struct ScopedAttribute {
    pub attribute: Attribute,
    /// The group that owns the views this column family is over — always the owner, a scope
    /// naming a `members` group being refused pointing at it.
    pub group: String,
    /// The column's **own** `source`, where it declares one (`views.md` §5). `None` — the shape
    /// Appendix A's `sentiment` and the fixture's declare — reads each view's column from that
    /// view's own points file, under that view's selection where a group's views share one.
    pub source: Option<ScopedAttributeFile>,
}

/// A group-scoped attribute's own source file (`views.md` §5): one row per `(entity, view)`, the
/// view named by a discriminator column.
///
/// **The discriminator is what makes a file of its own admissible at all.** A scoped column's
/// values are one per `(entity, view)`, so reading such a file as entity space would take one
/// arbitrary view's values as every view's — silently, and with no error anywhere. The
/// discriminator is the same mechanism a form B roster and a scoped layer's artifacts already use,
/// and it is applied here by the same [`ViewSelector`].
#[derive(Debug, Clone)]
pub struct ScopedAttributeFile {
    /// The resolved path of the `[sources]` entry the column named.
    pub path: PathBuf,
    /// The column that file spells the entity id in — `entity_id_field`, or the default.
    pub entity_id: String,
    /// The discriminator column — the attribute's `fields.view`, resolved; `view` by default.
    pub view_field: String,
}

/// Which attributes and which layers carry a group scope, by name.
///
/// **Beside the declarations rather than inside them**, on `Config::layer_sources`' precedent and
/// for a sharper reason: a [`LayerDeclaration`] is exactly what `PUT /control/layers` takes and an
/// [`Attribute`] is exactly what `MANIFEST.declared_scalars` carries, and neither contract has a
/// slot for a scope: a scoped attribute's record is `MANIFEST.groups[..].scoped_scalars`
/// (contracts §2.2) and ⊘ a scoped layer's has no home on the wire at all yet
/// (`views.md` §11). A scope written into either declaration would be a field no reader knows.
/// Entity scope — the default —
/// is absence from these maps rather than an entry, so nothing has to be written to say *the
/// ordinary thing*.
#[derive(Debug, Clone, Default)]
pub struct Scopes {
    /// Attribute name → the group its column family is over.
    pub attributes: BTreeMap<String, String>,
    /// Layer name → the group its artifact sets are per view of.
    pub layers: BTreeMap<String, String>,
}

impl Scopes {
    /// The group `attribute` is scoped to, or `None` for the entity-scoped default.
    pub fn attribute(&self, attribute: &str) -> Option<&str> {
        self.attributes.get(attribute).map(String::as_str)
    }

    /// The group `layer` is scoped to, or `None` for the entity-scoped default.
    pub fn layer(&self, layer: &str) -> Option<&str> {
        self.layers.get(layer).map(String::as_str)
    }
}

impl ViewGroup {
    /// The keys this group's views are declared under, where the declaration enumerates them.
    ///
    /// **Empty is not *no views*** — under a discriminator, and on a `members` group, the keys are
    /// the owner's or the data's and are not known from the declaration alone.
    pub fn declared_keys(&self) -> Vec<&str> {
        match &self.roster {
            Roster::Inline(views) => views.iter().map(|v| v.key.as_str()).collect(),
            Roster::Table(_) | Roster::Discriminator => Vec::new(),
        }
    }

    /// Which of the two roster forms was written, for a report to quote back.
    pub fn form(&self) -> &'static str {
        match &self.roster {
            Roster::Inline(_) => "form A, one points file per view",
            Roster::Table(_) => "form B, one points file and a roster table",
            Roster::Discriminator => "views minted from the discriminator",
        }
    }
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
    /// data, with `margin` of the data span added on each side. Resolved by [`frame_view`].
    Auto { margin: f64 },
    /// `{ min, max }` or `{ x = [a, b], y = [c, d] }` — stated outright, and the only form a
    /// corpus that will be written to should rely on.
    Fixed(Bounds),
    /// `{ lon = [a, b], lat = [c, d] }` — a **projected** view's frame, written where a caller
    /// can read it off an atlas, and snapped outward to the enclosing aligned square
    /// (`projections.md` §4.2).
    LonLat(LonLatBox),
    /// `"auto"` on a projected view: the same snap over the data's own longitude/latitude box.
    /// Distinct from [`Extent::Auto`] because a projected frame's headroom is the snap and never
    /// a fraction of the data span, so there is no margin to carry.
    AutoLonLat,
}

/// A box in longitude and latitude, degrees, WGS84 — the only frame spelling a projected view
/// takes (`projections.md` §4.2).
///
/// **Degenerate boxes are legal here and are not legal as a [`Bounds`]**: `lon = [a, a]` states a
/// meridian, which is a box with no smallest enclosing square, and §4.2 answers it with the offset
/// cap rather than a refusal. The frame it snaps to is always a proper square.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct LonLatBox {
    pub lon_min: f64,
    pub lon_max: f64,
    pub lat_min: f64,
    pub lat_max: f64,
}

impl LonLatBox {
    /// This box's image in the frame the projection produces.
    ///
    /// **Every projection in the set is cylindrical** — longitude maps linearly to x and latitude
    /// monotonically to y — so the image of a longitude/latitude rectangle is a rectangle and its
    /// corners are its bounds (`projections.md` §4.2). The y axis runs **south**, so the box's
    /// minimum latitude is its maximum y.
    pub fn project(&self, projection: Projection) -> Bounds {
        let (x_min, y_max) = projection.forward(self.lon_min, self.lat_min);
        let (x_max, y_min) = projection.forward(self.lon_max, self.lat_max);
        Bounds {
            x_min,
            x_max,
            y_min,
            y_max,
        }
    }
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
    /// Whose frame this is, already spelled for a message: `view 'world'`, or
    /// `view group 'quarter'` where one frame covers every view of a group (`views.md` §3.1).
    pub subject: String,
    /// What placed every position in this view, before the frame did (`projections.md` §3).
    pub projection: Projection,
    /// What every stored position in this view is quantised across. For a projected view this is
    /// an aligned square over the unit square rather than anything the caller wrote.
    pub extent: Bounds,
    /// The box the caller asked for, for a projected view that stated one. `None` under `auto`,
    /// where the box is the data's own, and for an unprojected view, which states its frame
    /// directly and has nothing to snap.
    pub asked: Option<LonLatBox>,
    /// The aligned square [`Frame::asked`] — or the data's own box — snapped outward to
    /// (`projections.md` §4.2). `None` for an unprojected view. The difference between the box and
    /// the square is resolution the corpus does not get, which is why it is carried rather than
    /// absorbed.
    pub snap: Option<Snap>,
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

    /// Points whose latitude fell outside the projection's own domain, so the transform moved
    /// them onto the frame's edge and lost the difference (`projections.md` §7).
    ///
    /// **Never a clamp, and never counted as one.** A clipped point lands exactly on the edge,
    /// where the quantisation rule says nothing is clamped, so the clamp counter structurally
    /// cannot see one however many there are. Zero for an unprojected view, which has no domain.
    pub fn clipped(&self) -> u64 {
        self.coordinates().map_or(0, |survey| survey.clipped)
    }

    /// **What the build says about this frame, every time, whether or not anything is wrong.**
    /// The projection that placed the points and the extent they were quantised across; for a
    /// projected view the box the caller asked for and the square it snapped to; the data's own
    /// bounds; how much of the grid that leaves the data occupying; how many points land on the
    /// boundary rather than where they were written; and, on its own line, how many the
    /// projection clipped at its own domain (`projections.md` §8).
    ///
    /// Reported rather than merely available: the whole defect this closes was a build that had
    /// every one of these numbers and printed none of them. **It never refuses** — the refusal
    /// this frame may earn is [`Frame::refusal`], and clipping is not among its causes (§7).
    pub fn report(&self) -> String {
        let e = &self.extent;
        // **The projection is named beside the frame, and only where there is one.** Under
        // `projection = "none"` this is the line every build has always printed, to the word: the
        // view's coordinates are its file's own, and naming an absent transform would put a word
        // in front of every existing corpus's frame for nothing.
        let mut out = match self.projection {
            Projection::None => format!(
                "{}: quantising against x [{}, {}], y [{}, {}]",
                self.subject, e.x_min, e.x_max, e.y_min, e.y_max
            ),
            projection => format!(
                "{}: {}, quantising against x [{}, {}], y [{}, {}]",
                self.subject,
                projection.name(),
                e.x_min,
                e.x_max,
                e.y_min,
                e.y_max
            ),
        };
        if let Some(snap) = &self.snap {
            out.push_str(&self.snap_line(snap));
        }
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
            // **The clamp counter alone cannot say the edge is empty.** A clipped point lands
            // exactly on the edge and is deliberately *not* clamped (`projections.md` §7), so
            // where anything was clipped this sentence would otherwise assert the opposite of the
            // clip line two below it. It narrows to the claim the clamp counter can actually
            // support, and the clip line makes the claim it cannot.
            let edge = if survey.clipped == 0 {
                "none on the frame's edge"
            } else {
                "none clamped onto the frame's edge"
            };
            out.push_str(&format!(
                "\n        {} point(s) placed, {edge}",
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
        // **Clipped points on their own line and in their own field** (`projections.md` §7). A
        // clipped point is stored on the frame's edge, which is exactly where the clamp rule says
        // a point is *not* clamped — so the counter above structurally cannot see one, and a
        // second number on the clamp line would hand a real count to the wrong cause. Printed
        // whether or not anything was clipped, for the same reason the frame is: silence has to
        // mean *nothing was clipped* rather than *nobody counted*.
        //
        // Exactly where the projection has a domain to fall outside of, which is every entry in
        // the set and not `none`.
        if let Some(domain) = self.projection.max_latitude_deg() {
            if survey.clipped == 0 {
                out.push_str(&format!(
                    "\n        none of them outside {}'s ±{domain}° domain, so nothing was clipped",
                    self.projection.name()
                ));
            } else {
                out.push_str(&format!(
                    "\n        {} of {} point(s) ({:.1}%) CLIPPED at {}'s ±{domain}° domain — \
                     stored on the frame's edge, not where they were written. Built anyway at any \
                     proportion: the domain is the projection's own boundary and no frame moves \
                     it, so a real tail beyond it is the wrong projection for this corpus rather \
                     than the wrong frame",
                    survey.clipped,
                    survey.rows,
                    survey.clipped as f64 / survey.rows as f64 * 100.0,
                    self.projection.name(),
                ));
            }
        }
        out
    }

    /// The box the caller asked for, the square it snapped to, and whether the offset cap chose
    /// that square rather than the box (`projections.md` §4.2, §8).
    ///
    /// **Both boxes in degrees, at full precision, whether or not the difference is large.** The
    /// snap is resolution the corpus does not get, and a caller who can see the box beside the
    /// frame is the one who can judge that; rounding the frame's own corners would print two
    /// different frames identically at the offsets where a cell is centimetres. The square's
    /// address is the frame's exact identity either way, and it is the address `tessera check`
    /// prints from the declaration alone (`crate::check::FramePreview`).
    fn snap_line(&self, snap: &Snap) -> String {
        let asked = match &self.asked {
            Some(b) => format!(
                "asked for lon [{}, {}], lat [{}, {}]",
                b.lon_min, b.lon_max, b.lat_min, b.lat_max
            ),
            // `auto` on a projected view: the box snapped is the data's own, and the data's box
            // is the line below this one.
            None => "`extent = \"auto\"` over the data's own box".to_string(),
        };
        let how = if snap.floored {
            "FLOORED at the offset cap rather than fitted: the square at"
        } else {
            "snapped outward to the square at"
        };
        // y runs south (`projections.md` §4), so the frame's minimum y is its maximum latitude.
        let (lon_min, lat_max) = self
            .projection
            .inverse(self.extent.x_min, self.extent.y_min);
        let (lon_max, lat_min) = self
            .projection
            .inverse(self.extent.x_max, self.extent.y_max);
        format!(
            "\n        {asked} — {how} z{} ({}, {}), lon [{lon_min}, {lon_max}], lat [{lat_min}, \
             {lat_max}]",
            snap.square.z, snap.square.x, snap.square.y
        )
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
            "{}: {} of {} point(s) ({:.1}%) would be stored on the frame's edge rather \
             than where they were written. The frame is x [{}, {}], y [{}, {}]; the data spans x \
             [{}, {}], y [{}, {}]. Past half the corpus this is not a tail, it is the wrong frame \
             — quantisation clamps rather than filters, so a bundle built here is well-formed \
             with the geometry wrong. Write `extent = \"auto\"` to fit the data, or state the box \
             the data is actually in; filter the source if the intent was to crop",
            self.subject,
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
    projection: Projection,
    extent: &Extent,
    points: &Path,
    fields: &Fields,
    limit: Option<u64>,
) -> Result<Frame> {
    frame_of(
        &format!("view '{view}'"),
        projection,
        extent,
        &[FrameSource {
            points,
            fields,
            select: None,
        }],
        limit,
    )
}

/// One source a frame is fitted over: a view's points, and the rows of that file which are its
/// own (`views.md` §3.1's form B).
#[derive(Debug, Clone, Copy)]
pub struct FrameSource<'a> {
    pub points: &'a Path,
    pub fields: &'a Fields,
    pub select: Option<&'a ViewSelector>,
}

/// [`frame_view`] over **several** sources, which is what a group's one frame is fitted to.
///
/// **One frame for the group** (`views.md` §3.1): its views differ by a key and by per-view
/// metadata and by nothing else, which is what makes a Morton prefix mean the same thing in each
/// of them. So `auto` on a group surveys every view's source and fits the box to the union of
/// their boxes — never one box per view, which would give each view its own grid under one
/// declaration — and a stated extent is surveyed against every one of them, so the clamp report
/// covers the whole group.
pub fn frame_of(
    name: &str,
    projection: Projection,
    extent: &Extent,
    sources: &[FrameSource],
    limit: Option<u64>,
) -> Result<Frame> {
    let survey_all = |against: Option<&Bounds>| -> Result<PointSurvey> {
        let mut surveys = Vec::with_capacity(sources.len());
        for source in sources {
            surveys.push(crate::input::survey_points(
                source.points,
                source.fields,
                projection,
                limit,
                source.select,
                against,
            )?);
        }
        union_surveys(name, surveys)
    };
    let margin = match extent {
        Extent::Fixed(bounds) => {
            let survey = survey_all(Some(bounds))?;
            return Ok(Frame {
                subject: name.to_string(),
                projection,
                extent: *bounds,
                asked: None,
                snap: None,
                survey,
            });
        }
        // **A stated longitude/latitude box needs no data to become a frame** — it is projected
        // and snapped here, and the pass that follows is the clamp survey every frame gets. That
        // is what lets `tessera check` answer the same question against no file at all
        // (`crate::check`).
        Extent::LonLat(asked) => {
            let snap = snap_lon_lat(projection, asked);
            let bounds = snap.square.bounds();
            let survey = survey_all(Some(&bounds))?;
            return Ok(Frame {
                subject: name.to_string(),
                projection,
                extent: bounds,
                asked: Some(*asked),
                snap: Some(snap),
                survey,
            });
        }
        // The projected `auto`: the same snap, over the data's own box rather than a stated one.
        // The survey runs in the frame the projection produces, so the box it comes back with is
        // already the thing to snap — projecting the corners of the degree-space box would give
        // the same square, every projection in the set being monotone on each axis.
        Extent::AutoLonLat => {
            let survey = survey_all(None)?;
            let PointSurvey::Coordinates(survey) = survey else {
                unreachable!("survey_points refuses a Morton source when no frame is supplied")
            };
            let data = survey.bounds.ok_or_else(|| empty_auto_source(name))?;
            let snap = snap_outward(&data);
            return Ok(Frame {
                subject: name.to_string(),
                projection,
                extent: snap.square.bounds(),
                asked: None,
                snap: Some(snap),
                survey: PointSurvey::Coordinates(survey),
            });
        }
        Extent::Auto { margin } => *margin,
    };
    let survey = survey_all(None)?;
    let PointSurvey::Coordinates(survey) = survey else {
        unreachable!("survey_points refuses a Morton source when no frame is supplied")
    };
    let data = survey.bounds.ok_or_else(|| empty_auto_source(name))?;
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
            "{name}: `extent = \"auto\"` fitted no usable box around the data \
             ({detail}). The data spans x [{}, {}], y [{}, {}]; state the frame outright if that \
             is not what this corpus is",
            data.x_min, data.x_max, data.y_min, data.y_max
        ))
    })?;
    Ok(Frame {
        subject: name.to_string(),
        projection,
        extent: bounds,
        asked: None,
        snap: None,
        survey: PointSurvey::Coordinates(survey),
    })
}

/// One frame's several sources, folded into the survey the report is printed from.
///
/// Boxes union, and every counter sums: the group's frame is judged against every row it will
/// place, so a clamp in one view is a clamp in the group's report. A build mixing coordinate and
/// Morton sources under one frame is refused naming the mixture — the two are quantised in
/// different places, so a shared frame would mean one thing for some of the group's views and
/// another for the rest.
fn union_surveys(name: &str, surveys: Vec<PointSurvey>) -> Result<PointSurvey> {
    let mut folded: Option<CoordinateSurvey> = None;
    let mut quantised = false;
    for survey in surveys {
        match survey {
            PointSurvey::Quantised => quantised = true,
            PointSurvey::Coordinates(one) => {
                folded = Some(match folded {
                    None => one,
                    Some(acc) => CoordinateSurvey {
                        rows: acc.rows + one.rows,
                        bounds: match (acc.bounds, one.bounds) {
                            (Some(a), Some(b)) => Some(Bounds {
                                x_min: a.x_min.min(b.x_min),
                                x_max: a.x_max.max(b.x_max),
                                y_min: a.y_min.min(b.y_min),
                                y_max: a.y_max.max(b.y_max),
                            }),
                            (a, b) => a.or(b),
                        },
                        clamped: acc.clamped + one.clamped,
                        clamped_x: acc.clamped_x + one.clamped_x,
                        clamped_y: acc.clamped_y + one.clamped_y,
                        clipped: acc.clipped + one.clipped,
                    },
                });
            }
        }
    }
    match (folded, quantised) {
        (Some(folded), false) => Ok(PointSurvey::Coordinates(folded)),
        (None, _) => Ok(PointSurvey::Quantised),
        (Some(_), true) => Err(declaration_error(format!(
            "'{name}': one frame is fitted over several sources (views §3.1), and some of them \
             carry coordinates while others carry Morton codes. A code is already placed in the \
             grid's own frame and a coordinate is quantised against this one, so the two cannot \
             share a frame. Give the group's views one geometry shape"
        ))),
    }
}

/// `auto` over a source that selects no rows: there is no data to fit a box around, on either
/// spelling, so the frame has to be stated (`projections.md` §4.2, `configuration.md` §1).
fn empty_auto_source(subject: &str) -> BuildError {
    declaration_error(format!(
        "{subject}: `extent` is `auto` and the points source selects no rows, so there is no \
         data to fit a box around. Either the source is empty or `--limit` excludes every row; \
         state the frame instead — `extent = {{ min = <a>, max = <b> }}`, or `extent = {{ lon = \
         [<a>, <b>], lat = [<c>, <d>] }}` under a projection — if this corpus is meant to start \
         empty and be written to"
    ))
}

/// A stated longitude/latitude box as a frame: projected, then snapped outward to the smallest
/// aligned square containing it (`projections.md` §4.2).
///
/// **No data is read**, which is what makes the frame a property of the declaration alone and lets
/// `tessera check` print it in seconds.
pub fn snap_lon_lat(projection: Projection, asked: &LonLatBox) -> Snap {
    snap_outward(&asked.project(projection))
}

/// Compile a view's `extent`, in whichever spellings its projection admits.
///
/// **The projection decides the spelling, and the two sets do not overlap.** An unprojected view
/// takes `configuration.md` §1's four, in the space its file is already in; a projected view takes
/// `auto` or a box in longitude and latitude, and nothing else (`projections.md` §4.2) — `min`/`max`
/// and `x`/`y` describe a frame in the space the projection *produces*, which is on the wrong side
/// of the transform, and `margin` is headroom the snap already supplies. Every refusal names the
/// spellings the view's own projection admits, because the key has no default and the value an
/// absent line would supply is a decision about where every stored point lands.
fn compile_extent(
    object: &str,
    projection: Projection,
    value: Option<&toml::Value>,
) -> Result<Extent> {
    let projected = projection != Projection::None;
    let spellings = if projected {
        LON_LAT_SPELLINGS
    } else {
        EXTENT_SPELLINGS
    };
    let Some(value) = value else {
        return Err(declaration_error(format!(
            "{object}: `extent` is required and has no default (configuration.md §1). It is \
             the frame every stored position is quantised across, and quantisation clamps — so a \
             guessed frame is a bundle that is well-formed with the geometry wrong. This view's \
             spellings:{spellings}"
        )));
    };
    if let Some(word) = value.as_str() {
        if word == "auto" {
            return Ok(if projected {
                Extent::AutoLonLat
            } else {
                Extent::Auto {
                    margin: DEFAULT_AUTO_MARGIN,
                }
            });
        }
        return Err(declaration_error(format!(
            "{object}: `extent = \"{word}\"` is not a value this key takes. The only word \
             it takes is `auto`; every other spelling is a table:{spellings}"
        )));
    }
    let Some(table) = value.as_table() else {
        return Err(declaration_error(format!(
            "{object}: `extent` is neither the word `auto` nor a table:{spellings}"
        )));
    };
    let table: ExtentTable = ExtentTable::deserialize(toml::Value::Table(table.clone()))
        .map_err(|e| declaration_error(format!("{object}: `extent`: {e}")))?;

    if projected {
        return compile_lon_lat_extent(object, projection, &table);
    }
    if table.lon.is_some() || table.lat.is_some() {
        return Err(declaration_error(format!(
            "{object}: `extent` is written in longitude and latitude, and this view declares \
             no projection — so there is nothing to turn a degree into a coordinate and the two \
             numbers would be quantised as though they were the file's own units \
             (projections.md §5.3). Declare `projection = \"web_mercator\"` or \
             `projection = \"equirectangular\"` if these are places on the Earth; otherwise state \
             the frame in the coordinates the file carries:{EXTENT_SPELLINGS}"
        )));
    }

    let stated =
        table.min.is_some() || table.max.is_some() || table.x.is_some() || table.y.is_some();
    if let Some(auto) = table.auto {
        if !auto {
            return Err(declaration_error(format!(
                "{object}: `extent = {{ auto = false }}` says what the frame is not. Write \
                 the frame:{}",
                EXTENT_SPELLINGS
            )));
        }
        if stated {
            return Err(declaration_error(format!(
                "{object}: `extent` declares `auto` and a stated box together. `auto` fits \
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
                        "{object}: `extent.margin = {margin}` is not a fraction of the data \
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
            "{object}: `extent.margin` without `auto = true`. A margin is headroom around a \
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
                "{object}: `extent` is an empty table, so it declares no frame at all:{}",
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
                "{object}: `extent` names {named}, which is half a frame. `min` and `max` \
                 give one range to both axes and preserve the aspect ratio; `x` and `y` give a \
                 range each, where stretching is meant. Neither half stands alone:{}",
                EXTENT_SPELLINGS
            )));
        }
    };
    bounds
        .validate()
        .map_err(|detail| declaration_error(format!("{object}: `extent`: {detail}")))?;
    Ok(Extent::Fixed(bounds))
}

/// A projected view's `extent`: `auto`, or the box in longitude and latitude
/// (`projections.md` §4.2). Every other spelling is refused here, naming this one.
fn compile_lon_lat_extent(
    object: &str,
    projection: Projection,
    table: &ExtentTable,
) -> Result<Extent> {
    let name = projection.name();
    if table.auto.is_some() || table.margin.is_some() {
        return Err(declaration_error(format!(
            "{object}: `extent` declares `auto` as a table, and the view is projected \
             ({name}). A projected frame's headroom is the outward snap to the enclosing aligned \
             square, not a fraction of the data span — and a margin inside an aligned square would \
             only shrink the frame away from the alignment it exists to have \
             (projections.md §4.2). Write `extent = \"auto\"`, which fits the data's own \
             longitude/latitude box and snaps it:{LON_LAT_SPELLINGS}"
        )));
    }
    if table.min.is_some() || table.max.is_some() || table.x.is_some() || table.y.is_some() {
        let named = [
            table.min.map(|_| "min"),
            table.max.map(|_| "max"),
            table.x.map(|_| "x"),
            table.y.map(|_| "y"),
        ]
        .into_iter()
        .flatten()
        .collect::<Vec<_>>()
        .join(", ");
        return Err(declaration_error(format!(
            "{object}: `extent` names {named}, and this view is projected ({name}). Those \
             spellings state a frame in the space the projection *produces*, which is the output \
             of a calculation nobody should do by hand — and getting it wrong misplaces every \
             stored position. A projected view's frame is written in degrees, where a caller can \
             read it off an atlas (projections.md §4.2):{LON_LAT_SPELLINGS}"
        )));
    }
    let (Some(lon), Some(lat)) = (table.lon, table.lat) else {
        let half = match (table.lon, table.lat) {
            (Some(_), None) => "`lon` without `lat`",
            (None, Some(_)) => "`lat` without `lon`",
            _ => "no frame at all",
        };
        return Err(declaration_error(format!(
            "{object}: `extent` is {half}. A projected view's frame is a box in longitude \
             and latitude and neither half stands alone:{LON_LAT_SPELLINGS}"
        )));
    };
    for (axis, limit, pair) in [("lon", 180.0, lon), ("lat", 90.0, lat)] {
        for v in pair {
            if !v.is_finite() || v.abs() > limit {
                return Err(declaration_error(format!(
                    "{object}: `extent.{axis}` names {v}, which is not a {}. The accepted \
                     input coordinate system is WGS84 degrees — longitude within ±180, latitude \
                     within ±90 (projections.md §2) — and a value outside that is not a \
                     coordinate. Convert the box to WGS84, or declare `projection = \"none\"` if \
                     this view's space is not the Earth",
                    if axis == "lon" {
                        "longitude"
                    } else {
                        "latitude"
                    }
                )));
            }
        }
    }
    if lon[0] > lon[1] {
        return Err(declaration_error(format!(
            "{object}: `extent.lon = [{}, {}]` runs west from its own maximum. Read as a box \
             crossing the antimeridian it cannot be honoured — a frame is one aligned square and \
             an aligned square does not wrap — and read as an ordinary box it is inverted. Write \
             the wider box that does not cross: `lon = [{}, {}]`",
            lon[0], lon[1], lon[1], lon[0]
        )));
    }
    if lat[0] > lat[1] {
        return Err(declaration_error(format!(
            "{object}: `extent.lat = [{}, {}]` runs south from its own maximum, so it names \
             no box. Latitude does not wrap; write `lat = [{}, {}]`",
            lat[0], lat[1], lat[1], lat[0]
        )));
    }
    Ok(Extent::LonLat(LonLatBox {
        lon_min: lon[0],
        lon_max: lon[1],
        lat_min: lat[0],
        lat_max: lat[1],
    }))
}

/// A projected view's two spellings, appended to every `extent` refusal it earns.
const LON_LAT_SPELLINGS: &str = "\n  \
     extent = \"auto\"                                         # the data's own box, snapped\n  \
     extent = { lon = [-8.6, 1.8], lat = [49.9, 60.9] }      # stated in degrees, snapped outward";

/// The four spellings, appended to every `extent` refusal. A caller who got this key wrong is
/// choosing between four shapes, not correcting a typo, so the whole set travels with the message.
const EXTENT_SPELLINGS: &str = "\n  \
     extent = \"auto\"                          # the square box around the data, small margin\n  \
     extent = { auto = true, margin = 0.25 }  # a quarter of the data span as headroom each side\n  \
     extent = { min = -25.0, max = 25.0 }     # one range, both axes — preserves aspect ratio\n  \
     extent = { x = [-18, 19], y = [-22, 24] }  # per axis, where stretching is meant";

/// Where a build reads each point's access terms, and what a point carrying none is given.
///
/// **The three shapes are one declaration, not three routes.** The acquisition half is optional,
/// and a corpus with no permission model is the one that declares only a default. `default` is
/// optional too (decision 0133), and its absence is a decision: a point carrying no terms is then
/// refused, at the build and at `/control/ingest` alike, naming the count.
#[derive(Debug, Clone)]
pub struct AccessInput {
    pub source: AccessSource,
    /// What a point carrying no terms of its own is given, or `None` to refuse such a point.
    /// Never `inherited` (§1); any other string is a term, commas and all — the plugin is handed
    /// a list, so nothing splits it.
    pub default: Option<String>,
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
            default: Some(
                String::from_utf8(tessera_authz::PUBLIC_LABEL.to_vec())
                    .expect("the reserved label is ASCII"),
            ),
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
    /// `None` where the declaration names no default, and a point carrying no terms is then
    /// refused at both entry points (decision 0133).
    pub default: Option<String>,
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
        // Groups after the vocabularies a category's metadata draws on, and after the views whose
        // names a group may not share; before the attributes and layers whose `scope` names one.
        let view_groups =
            compile_view_groups(&file.view_group, &views, &vocabularies, &sources, &defaults)?;
        // The scopes first: which attributes are entity space and which are a family is what
        // decides the schema itself (`views.md` §5).
        let attribute_scopes = compile_attribute_scopes(&file.attribute, &view_groups)?;
        let (attributes, scoped_attributes) = compile_attributes(
            &file.attribute,
            &vocabularies,
            &attribute_scopes,
            &sources,
            &defaults,
        )?;
        let attribute_sources =
            compile_attribute_sources(&file.attribute, &attribute_scopes, &sources, &defaults)?;
        let (layers, layer_sources, label_layers, layer_scopes) =
            compile_layers(&file.layer, &views, &view_groups, &attributes, &sources)?;

        Ok(Config {
            schema: Schema {
                attributes,
                vocabularies,
            },
            attribute_sources,
            views,
            view_groups,
            scopes: Scopes {
                attributes: attribute_scopes,
                layers: layer_scopes,
            },
            scoped_attributes,
            layers,
            // Declaration order, read off the blocks themselves: the compiled halves are two
            // lists, and a map of vocabularies has no order at all.
            attribute_order: file.attribute.iter().map(|b| b.name.clone()).collect(),
            vocabulary_order: file.vocabulary.iter().map(|b| b.name.clone()).collect(),
            vocabulary_sources: file
                .vocabulary
                .iter()
                .filter_map(|b| b.source.clone().map(|s| (b.name.clone(), s)))
                .collect(),
            layer_sources,
            label_layers,
            allocation_view: defaults.allocation_view.clone(),
        })
    }

    /// The entity-space files this build reads: the attribute sources and the layers.
    ///
    /// **Where the declaration meets the invocation.** Everything above is route-independent: the
    /// same blocks describe a deployment that never builds (`configuration.md` §2), and a config
    /// declaring no source at all is legal and declares an empty corpus. This is the method that
    /// asks for the files, so it is where *this* build's absences become refusals.
    ///
    /// Each view's own half — its geometry source and its labels — is [`acquire_view`], because a
    /// view owns everything downstream of the permutation and nothing upstream of it
    /// (`views.md` §1).
    pub fn acquire(&self) -> Result<Acquisition> {
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
            layers: self.layer_sources.clone(),
        })
    }
}

/// One view's own inputs: its geometry source and where its points' labels come from.
///
/// **Form B's selection travels with the file** (`views.md` §3.1): a view whose points sit in a
/// shared source carries the discriminator here, and every pass over that file — the id union,
/// the labels, the geometry, the survey — applies it, so no view ever reads another's rows into
/// its own row space.
pub fn acquire_view(view: &BuildView) -> Result<ViewAcquisition> {
    let points = view.source.clone().ok_or_else(|| {
        declaration_error(format!(
            "view '{}': `source` is required to build from a file (configuration.md §1). \
             It is the path — relative to this config — of this view's geometry: `entity_id` \
             with either `x`/`y` or `morton`/`residual`. ⊘ Declaring no source is legal and \
             means the view is declared and empty, which is a bundle with no rows in it (§2) \
             and is not built",
            view.id
        ))
    })?;
    Ok(ViewAcquisition {
        points,
        point_fields: view.fields.clone(),
        select: view.select.clone(),
        access: AccessInput {
            source: match (&view.point_visibility.source, &view.point_visibility.field) {
                (Some(path), _) => AccessSource::Relation(path.clone()),
                (None, Some(field)) => AccessSource::Field(field.clone()),
                // Legal, and the corpus with no permission model: every point takes the default
                // (§1). *Nowhere* is the decision the `default` key makes, so nothing is refused
                // here — a build with neither acquisition key reads no relation and writes the one
                // label the declaration named.
                (None, None) => AccessSource::Default,
            },
            default: view.point_visibility.default.clone(),
        },
    })
}

/// One view's resolved inputs ([`acquire_view`]).
#[derive(Debug, Clone)]
pub struct ViewAcquisition {
    /// The view's `source`: identity and geometry.
    pub points: PathBuf,
    /// Where the view's identity and geometry fields sit in that file.
    pub point_fields: Fields,
    /// Which of that file's rows are this view's, where the file holds several views'
    /// (`views.md` §3.1's form B). `None` where the file is the view.
    pub select: Option<ViewSelector>,
    /// Where this view's points get their access terms, and what a point carrying none gets.
    pub access: AccessInput,
}

/// The files one build reads, resolved from the config and any `--file` overrides, plus the
/// frame it quantises against.
#[derive(Debug, Clone)]
pub struct Acquisition {
    /// The declared attributes grouped by the file each is read from — one pass per group, joined
    /// to entity space by the identity column each group names. Empty for an empty schema; an
    /// attribute with no file to read it from is refused at parse.
    pub attribute_sources: Vec<AttributeSource>,
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
    /// `[defaults].allocation_view` as written. Resolved against the built view registry by
    /// [`Config::anchor_view`], not here: the groups are compiled after `[defaults]` is.
    allocation_view: Option<String>,
}

impl Defaults {
    fn compile(block: Option<&DefaultsBlock>, sources: &Sources) -> Result<Defaults> {
        let Some(block) = block else {
            return Ok(Defaults {
                source: None,
                entity_id_field: ENTITY_ID.to_string(),
                allocation_view: None,
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
            allocation_view: block.allocation_view.clone(),
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
    /// Owned rather than `&'static str`: a `[view_group.views]` roster's known fields are the
    /// group's own declared metadata names, which exist only for the length of a parse.
    name: String,
    /// `None` when this object always has the field; `Some(why)` when it does not have it here,
    /// `why` naming the key that would declare one.
    undeclared: Option<String>,
}

impl KnownField {
    /// A field the object always has.
    fn always(name: impl Into<String>) -> KnownField {
        KnownField {
            name: name.into(),
            undeclared: None,
        }
    }

    /// A field another key asserts the existence of: present when `declared`, and refused with
    /// `why` when it is not.
    fn asserted_by(name: impl Into<String>, declared: bool, why: &str) -> KnownField {
        KnownField {
            name: name.into(),
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
        let Some(field) = known.iter().find(|f| f.name == *canonical) else {
            return Err(declaration_error(format!(
                "{object}: `fields.{canonical}` is not one of this object's fields. They are: {}. \
                 The map says where a field is and never whether there is one, so a name outside \
                 the set is refused rather than passed to the reader",
                names(known.iter().map(|f| f.name.as_str()))
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
            // **The parent's scope, for the same reason as its views**: a label is drawn where the
            // thing it labels is drawn, so a label over a group-scoped layer is per view exactly
            // as its parent is. The sugar carries no `scope` key of its own — there is nothing a
            // label could be scoped to that its parent is not.
            scope: parent.scope.clone(),
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
            default_space: None,
            content: Some(ContentBlock {
                computed: Vec::new(),
                supplied: vec![SuppliedBlock {
                    name: labels.name.clone(),
                    ty: Some(ty),
                    require_member_visibility: Some(content_requirement),
                }],
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
    scopes: &BTreeMap<String, String>,
    sources: &Sources,
    defaults: &Defaults,
) -> Result<Vec<AttributeSource>> {
    let mut groups: Vec<AttributeSource> = Vec::new();
    // **The index recorded is the *schema's*, not the block's.** A group-scoped attribute is a
    // column family and is not in the schema at all (`views.md` §5), so the two spaces differ the
    // moment a scoped block is declared ahead of an entity-scoped one — and the scalar tail is
    // stored positionally, which is what a shifted index would silently rewrite.
    let mut schema_index = 0usize;
    for block in blocks {
        let object = format!("attribute '{}'", block.name);
        let index = schema_index;
        if !scopes.contains_key(&block.name) {
            schema_index += 1;
        }
        // **`[defaults].source` does not reach a group-scoped attribute** (`views.md` §5). Its
        // values are one per `(entity, view)` and are read from each view's own points file —
        // which is what Appendix A's `sentiment` does — and the default, a single whole-corpus
        // file, is exactly the wrong file. Taking it would group the column against a source
        // carrying one value per entity and report the column missing from it.
        if scopes.contains_key(&block.name) {
            continue;
        }
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
        check_view_name(&format!("view '{}'", block.name), &block.name)?;
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
        let projection = compile_projection(&object, block.projection.as_deref())?;
        // **A projected view's coordinate columns are `lon` and `lat`, and it has no other
        // geometry shape** (`projections.md` §2). An unprojected view keeps the two shapes it has
        // always had, mutually exclusive: a row carries `x`/`y` or `morton`/`residual`, and a map
        // naming one of each says the file carries both — which the reader would resolve by
        // preferring one, silently, over a declaration that asked for the other.
        let fields = if projection == Projection::None {
            compile_unprojected_fields(
                &object,
                source.as_ref(),
                block.fields.as_ref(),
                defaults,
                Vec::new(),
            )?
        } else {
            compile_projected_fields(
                &object,
                source.as_ref(),
                block.fields.as_ref(),
                defaults,
                Vec::new(),
            )?
        };
        let extent = compile_extent(&object, projection, block.extent.as_ref())?;
        let declared = block
            .visibility
            .clone()
            .map(tessera_types::view::DeclaredGate::into_labels);
        let visibility = compile_view_gate(&object, declared.as_deref())?;

        let point_visibility =
            compile_point_visibility(&object, block.point_visibility.as_ref(), sources)?;

        views.push(View {
            name: block.name.clone(),
            title: block.title.clone(),
            projection,
            source,
            fields,
            extent,
            visibility,
            point_visibility,
        });
    }
    Ok(views)
}

/// A view's `projection`, from the closed set of `projections.md` §5 and defaulting to `none`.
///
/// **The set is closed and the default is no projection**, so a corpus with no geography is never
/// asked to name one. A name outside the set is refused listing the set: the arithmetic of a
/// projection is part of the stored format — geometry is quantised against a declared frame and
/// the artifact *is* the record — so there is no reading of an unknown name that could be
/// approximated safely.
fn compile_projection(object: &str, declared: Option<&str>) -> Result<Projection> {
    let Some(name) = declared else {
        return Ok(Projection::None);
    };
    Projection::from_name(name).ok_or_else(|| {
        declaration_error(format!(
            "{object}: `projection = \"{name}\"` is not one of the projections this service \
             transforms with. They are: web_mercator, equirectangular, plate_carree, \
             gall_isographic, none (projections.md §5). The set is closed and stays cylindrical — \
             a conic or azimuthal entry would stop a longitude/latitude rectangle being a \
             rectangle, which is what lets an extent be written in degrees — and no datum shift, \
             national grid or caller-supplied projection is accepted"
        ))
    })
}

/// An unprojected view's or group's `fields`: `entity_id` with either `x`/`y` or
/// `morton`/`residual` (`configuration.md` §1), plus whatever `extra` its own block declares.
///
/// **The two geometry shapes are mutually exclusive**: a row carries coordinates or a code, so a
/// map naming one of each says the file has two geometries and leaves the reader to pick. The
/// geographic spellings are refused here rather than reported as unknown fields, because a caller
/// writing `fields.lon` on a view with no projection has said what their columns hold and been
/// given a frame that quantises degrees as though they were the file's own units.
fn compile_unprojected_fields(
    object: &str,
    source: Option<&PathBuf>,
    declared_fields: Option<&BTreeMap<String, String>>,
    defaults: &Defaults,
    extra: Vec<KnownField>,
) -> Result<Fields> {
    if let Some(declared) = declared_fields {
        for (geographic, axis) in [("lon", "x"), ("lat", "y")] {
            if declared.contains_key(geographic) {
                return Err(declaration_error(format!(
                    "{object}: `fields.{geographic}` on a view that declares no projection. There \
                     is nothing to turn a degree into a coordinate, so the column would be \
                     quantised as though it were the file's own units (projections.md §5.3). \
                     Declare `projection = \"web_mercator\"` or `projection = \"equirectangular\"` \
                     if these are places on the Earth; otherwise write `fields.{axis}`"
                )));
            }
        }
    }
    let mut known = vec![
        KnownField::always(ENTITY_ID),
        KnownField::always("x"),
        KnownField::always("y"),
        KnownField::always("morton"),
        KnownField::always("residual"),
    ];
    known.extend(extra);
    let fields = check_fields(
        object,
        source,
        &known,
        declared_fields,
        &defaults.entity_id_field,
    )?;
    if let Some(declared) = declared_fields {
        let quantised = declared.contains_key("x") || declared.contains_key("y");
        let coded = declared.contains_key("morton") || declared.contains_key("residual");
        if quantised && coded {
            return Err(declaration_error(format!(
                "{object}: `fields` names both an `x`/`y` pair and a `morton` code, and the two \
                 geometry shapes are mutually exclusive (configuration.md §1). A row carries \
                 coordinates or a code, so naming both says the source has two geometries and \
                 leaves the reader to pick"
            )));
        }
        if declared.contains_key("residual") && !declared.contains_key("morton") {
            return Err(declaration_error(format!(
                "{object}: `fields.residual` without `fields.morton`. A residual is the sub-cell \
                 remainder of a Morton code and is read only beside one"
            )));
        }
    }
    Ok(fields)
}

/// A projected view's `fields`: `entity_id` with `lon` and `lat`, and nothing else
/// (`projections.md` §2).
///
/// **The resolved map keys the coordinates on the canonical `x`/`y`**, so every reader below this
/// point sees a coordinate pair and the axis names are a property of the declaration alone. What
/// `lon`/`lat` buy is at the declaration: longitude-then-latitude is the order GeoJSON and WKT
/// use and the opposite of the order many sources publish, and a corpus built with the two
/// exchanged is silently mirrored about the diagonal. Naming the axes for what they hold removes
/// the ambiguity rather than documenting it.
fn compile_projected_fields(
    object: &str,
    source: Option<&PathBuf>,
    declared_fields: Option<&BTreeMap<String, String>>,
    defaults: &Defaults,
    extra: Vec<KnownField>,
) -> Result<Fields> {
    if let Some(declared) = declared_fields {
        for (axis, geographic) in [("x", "lon"), ("y", "lat")] {
            if declared.contains_key(axis) {
                return Err(declaration_error(format!(
                    "{object}: `fields.{axis}` on a projected view. A projected view's \
                     coordinate columns are `lon` and `lat` — longitude then latitude, the order \
                     GeoJSON and WKT use — because a corpus built with the two exchanged is \
                     silently mirrored about the diagonal (projections.md §2). Write \
                     `fields.{geographic}` instead"
                )));
            }
        }
        for coded in ["morton", "residual"] {
            if declared.contains_key(coded) {
                return Err(declaration_error(format!(
                    "{object}: `fields.{coded}` on a projected view. A Morton code is a \
                     position already placed in a frame, so there is no longitude for a \
                     projection to transform (projections.md §3). Either declare \
                     `projection = \"none\"` and read the codes against the grid's own frame, or \
                     supply `lon`/`lat` columns"
                )));
            }
        }
    }
    let mut known = vec![
        KnownField::always(ENTITY_ID),
        KnownField::always("lon"),
        KnownField::always("lat"),
    ];
    known.extend(extra);
    let fields = check_fields(
        object,
        source,
        &known,
        declared_fields,
        &defaults.entity_id_field,
    )?;
    // `lon` and `lat` become the canonical `x` and `y`, defaulting to their own names — which is
    // what makes `lon`/`lat` the columns a projected view reads with no `fields` map at all.
    let mut map = fields.map;
    for (axis, geographic) in [("x", "lon"), ("y", "lat")] {
        let column = map
            .remove(geographic)
            .unwrap_or_else(|| geographic.to_string());
        map.insert(axis.to_string(), column);
    }
    Ok(Fields {
        object: fields.object,
        map,
    })
}

/// A word written where an access label goes. `public` is a label and is fine; `inherited` is the
/// one reserved word occupying such a slot (§4), so it is refused rather than interned.
///
/// `object` is the declaration quoted as its own block names it — `view 's0'`, `layer
/// 'clusters/a'`, `view group 'quarter'` — because the same key is written on four kinds of block
/// and *which one* is half of the refusal.
fn check_label(object: &str, key: &str, label: &str) -> Result<()> {
    if label.trim().is_empty() {
        return Err(declaration_error(format!(
            "{object}: `{key}` is empty. An access label is a term a principal either holds or \
             does not; write `public` for the one every principal holds"
        )));
    }
    if label == INHERITED {
        return Err(declaration_error(format!(
            "{object}: an access label may not be spelled `inherited` — it is reserved for *the \
             container's gate is the whole of it*, and it is the one reserved word occupying a \
             slot that otherwise takes a label (configuration.md §4). `public` is not reserved in \
             this sense: it *is* a label, held by every principal"
        )));
    }
    Ok(())
}

// ---------------------------------------------------------------------------------------------
// View groups
// ---------------------------------------------------------------------------------------------

/// A name a view id is built out of — a plain view's `name`, or a group's `name` or one of its
/// keys (`views.md` §3.2).
///
/// **The same charset a column name takes**, and for the same reason: a view id addresses a
/// directory in the bundle (`views/<group>/<key>/`), the `view` in a request body, the
/// `x-tessera-view` header and the manifest's `files` map, so it has to survive being a path
/// segment. Two characters are refused ahead of the charset because they are *reserved* rather
/// than merely outside it: `:` joins a group to its key, and `@` pins a group-scoped attribute to
/// a view (`views.md` §5). Each
/// is refused here and again at manifest load, which are the two halves decision 0108 asks for.
fn check_view_name(object: &str, name: &str) -> Result<()> {
    // **The charset lives with the roster records** (`tessera_types::view::check_view_key`), so
    // the declaration and the create operation coin keys under one rule rather than two copies of
    // it — decision 0091's reading applied to a name.
    tessera_types::view::check_view_key(name)
        .map_err(|detail| declaration_error(format!("{object}: {detail}")))
}

/// A `[[view]]`'s, a `[[view_group]]`'s or a roster record's own `visibility` (`views.md` §6):
/// a list of labels, each one term taken verbatim (decision 0132). A declaration spells one
/// label as a string and several as a list; both arrive here as the list.
///
/// **`public` compiles to `None`**, which is what every downstream reader takes as *no gate*: it
/// is the label every principal holds inside the trust boundary (decision 0088), so storing the
/// word and storing nothing are the same statement and the shorter one cannot be misread as a
/// term to look up. It is recognised only as the whole of the list: beside another label it
/// would be a gate everybody passes, spelled as if it were narrower, so that is refused.
///
/// **Any other list is compiled after the plugin has been asked to read it** — the same question
/// an item's `access` list is put through at ingest ([`Plugin::terms_of_labels`]), because a view
/// gate is satisfied by exactly the item-visibility predicate (`views.md` §6) and a label the
/// plugin cannot read is one no principal could ever satisfy. Refusing it here is the difference
/// between a typo an author fixes at the build and a view that is silently reachable by nobody.
/// An empty list, and an empty element, are refused for the same reason: an empty term set
/// intersects nothing and gates the view against every principal including the one who wrote
/// it, and an empty element is no label.
///
/// The plugin asked is `builtin:passthrough`, which is the only one a build runs
/// (`tessera_build::build`); a deployment serving the bundle under a different plugin is a
/// mismatch the gate fails closed on rather than one this check could anticipate.
fn compile_view_gate(object: &str, declared: Option<&[String]>) -> Result<Option<Vec<String>>> {
    let Some(labels) = declared else {
        return Ok(None);
    };
    if labels == [PUBLIC] {
        return Ok(None);
    }
    if labels.is_empty() {
        return Err(declaration_error(format!(
            "{object}: `visibility = []` names no terms. A gate is satisfied where its term set \
             meets the principal's, so an empty one is satisfied by nobody and the view would be \
             reachable by no principal at all — including this build's author. Write `public`, \
             or the labels the gate names, one per element"
        )));
    }
    if labels.iter().any(|l| l == PUBLIC) {
        return Err(declaration_error(format!(
            "{object}: `visibility = {labels:?}` lists `public` beside another label. `public` \
             is the label every principal holds, so a gate naming it is satisfied by everybody; \
             write `public` alone, or leave it out of the list"
        )));
    }
    for (i, label) in labels.iter().enumerate() {
        if label.is_empty() {
            return Err(declaration_error(format!(
                "{object}: element {i} of `visibility = {labels:?}` is empty. Each element is \
                 one label a principal holds, taken as written, and an empty one is no label. \
                 Write `public`, or the labels the gate names, one per element"
            )));
        }
        check_label(object, "visibility", label)?;
    }
    let descriptors: Vec<Vec<u8>> = labels.iter().map(|l| l.as_bytes().to_vec()).collect();
    let descriptors = tessera_plugin::Passthrough::new()
        .terms_of_labels(&descriptors)
        .map_err(|e| {
            declaration_error(format!(
                "{object}: `visibility = {labels:?}` is not a label list the plugin can read \
                 ({e}). A view's gate is satisfied by the item-visibility predicate (views §6), \
                 so a label the plugin cannot turn into a term is one no principal could satisfy"
            ))
        })?;
    if descriptors.is_empty() {
        return Err(declaration_error(format!(
            "{object}: `visibility = {labels:?}` names no terms. A gate is satisfied where its \
             term set meets the principal's, so an empty one is satisfied by nobody and the view \
             would be reachable by no principal at all — including this build's author. Write \
             `public`, or labels naming terms"
        )));
    }
    Ok(Some(labels.to_vec()))
}

/// Compile every `[[view_group]]` (`views.md` §3, decision 0108).
///
/// **Two passes over the blocks**, because `members` may name a group declared after the one
/// naming it: the first settles the names — charset, duplicates, and the collision with a plain
/// view — and the second compiles each group against the whole set. Requiring declaration order
/// would be the alternative, and it would be an ordering nothing else in this surface has: a
/// layer's `depends_on` is ordered because registration is ordered, and a roster is not registered
/// at all.
fn compile_view_groups(
    blocks: &[ViewGroupBlock],
    views: &[View],
    vocabularies: &HashMap<String, Vocabulary>,
    sources: &Sources,
    defaults: &Defaults,
) -> Result<Vec<ViewGroup>> {
    let mut seen: HashSet<&str> = HashSet::new();
    for block in blocks {
        let object = format!("view group '{}'", block.name);
        check_view_name(&object, &block.name)?;
        if !seen.insert(block.name.as_str()) {
            return Err(declaration_error(format!(
                "{object} is declared twice. A group's name is half of every one of its views' ids \
                 (`<group>:<key>`), and a key is tombstoned on drop and never reused, so two \
                 blocks of one name is not a last-one-wins config question"
            )));
        }
        // **One namespace for views and groups.** A layer's `views` list and a `scope`'s `group`
        // take either word, so one name for both would make each of those mean two things — and a
        // group is not a view: it has no row space and cannot be named on a viewer verb.
        if views.iter().any(|v| v.name == block.name) {
            return Err(declaration_error(format!(
                "{object} has the name of a `[[view]]` block. A layer's `views` list and an \
                 attribute's `scope = {{ group = … }}` name either kind, so one word for both is \
                 ambiguous wherever they meet — and the two are not interchangeable: a group has \
                 no row space and cannot be named on a viewer verb (views §3.1)"
            )));
        }
    }

    let mut compiled = Vec::with_capacity(blocks.len());
    for block in blocks {
        compiled.push(compile_view_group(
            block,
            blocks,
            vocabularies,
            sources,
            defaults,
        )?);
    }
    Ok(compiled)
}

fn compile_view_group(
    block: &ViewGroupBlock,
    blocks: &[ViewGroupBlock],
    vocabularies: &HashMap<String, Vocabulary>,
    sources: &Sources,
    defaults: &Defaults,
) -> Result<ViewGroup> {
    let object = format!("view group '{}'", block.name);
    let form_a = !block.view.is_empty();
    let form_b = block.views.is_some();

    // **The roster is declared once**, and the two forms say different things about where the
    // points are: form A's file *is* the view, form B's source carries every view's rows behind a
    // discriminator. A group writing both has said the points are in two places and left the build
    // to choose — the refusal `source` beside inline `artifacts` earns, for the same reason.
    if form_a && form_b {
        return Err(declaration_error(format!(
            "{object} declares both roster forms — {} `[[view_group.view]]` block(s) and a \
             `[view_group.views]` table (views §3.1). The roster decides where the points come \
             from: under `[[view_group.view]]` each view names its own file and the group names \
             none, and under `[view_group.views]` the group's own `source` holds every view's \
             points with `fields.view` saying which view each row lands in. Write one of them",
            block.view.len()
        )));
    }

    // **`members` first**, because it decides which of the keys below this group may declare at
    // all: keys, metadata and each view's own gate belong to the group that owns them.
    let members = match &block.members {
        None => None,
        Some(target) => {
            if target == &block.name {
                return Err(declaration_error(format!(
                    "{object}: `members = \"{target}\"` names the group itself. `members` says \
                     this group's views are *another* group's (views §3.3), so a group naming \
                     itself has declared no views at all"
                )));
            }
            let owner = blocks.iter().find(|b| &b.name == target).ok_or_else(|| {
                declaration_error(format!(
                    "{object}: `members = \"{target}\"` names no `[[view_group]]` block. \
                     Declared: {}. A group takes another group's views by naming it, so a name \
                     nothing declares leaves this group with no views rather than with its own",
                    names(blocks.iter().map(|b| b.name.as_str()))
                ))
            })?;
            // **Chains are refused, so the owner of a key set is always one hop away** (§3.3).
            // Two hops would make *which group owns this key* a graph walk, and the owner is what
            // decides where a create lands and what a drop takes with it.
            if let Some(further) = &owner.members {
                return Err(declaration_error(format!(
                    "{object}: `members = \"{target}\"` names a group that itself declares \
                     `members = \"{further}\"`, and chains are refused (views §3.3). The owner of \
                     a key set is always one hop away, so a create and a drop resolve against one \
                     group rather than walking a graph. Name '{further}' here instead"
                )));
            }
            if block.metadata.is_some() {
                return Err(declaration_error(format!(
                    "{object}: `metadata` on a group declaring `members = \"{target}\"`. Keys \
                     and metadata belong to the group that owns the views, and these are \
                     '{target}'s (views §3.3) — a second typed value under one key would be a \
                     second roster for one key set. Declare it on '{target}'"
                )));
            }
            if form_a || form_b {
                return Err(declaration_error(format!(
                    "{object}: a roster on a group declaring `members = \"{target}\"`. Its views \
                     are '{target}'s, so the keys are declared there and a roster here would be a \
                     second one (views §3.3). Its points come from its own `source`, with \
                     `fields.view` naming the discriminator; its own `visibility` is the one gate \
                     it may still declare, a second layout being allowed to be narrower than the \
                     first"
                )));
            }
            Some(target.clone())
        }
    };

    // **Form A declares no group-level `source`, and the other two require one.** `[defaults]`
    // does not reach here at all: a defaulted group source would turn a form A declaration into a
    // form B one, or mint views from a discriminator column nobody named — an acquisition nobody
    // wrote, which is the line `configuration.md` §1 draws around `[defaults].source`.
    if form_a && block.source.is_some() {
        return Err(declaration_error(format!(
            "{object}: `source` beside `[[view_group.view]]` blocks. Under that roster each view's \
             points are that view's own `source` and the group declares none — the file is the \
             view, exactly as a layer's file is the layer (views §3.1). Move the file onto the \
             view whose points it holds, or write the roster as `[view_group.views]` beside one \
             `source` carrying every view's rows with `fields.view` as the discriminator"
        )));
    }
    if form_b && block.source.is_none() {
        return Err(declaration_error(format!(
            "{object}: a `[view_group.views]` roster and no `source`. The two are separate files \
             and the group needs both: the roster is one row per view, and the group's own \
             `source` holds every view's points with `fields.view` saying which view each row \
             lands in (views §3.1). A file per view instead is `[[view_group.view]]`, where the \
             group declares no `source` at all"
        )));
    }
    if !form_a && block.source.is_none() {
        return Err(declaration_error(format!(
            "{object}: no `source` and no `[[view_group.view]]` roster, so this group's points \
             come from nowhere. Either name a file per view under `[[view_group.view]]`, or name \
             the group's own `source` whose `fields.view` says which view each row lands in — with \
             `[view_group.views]` to list the views and their metadata, or without it to mint them \
             from the discriminator's distinct values (views §3.1). `[defaults].source` \
             deliberately does not reach a group: which of those two a defaulted file meant is not \
             something a default can decide"
        )));
    }

    let projection = compile_projection(&object, block.projection.as_deref())?;
    let source = match &block.source {
        Some(declared) => Some(sources.path(&object, declared)?),
        None => None,
    };
    // **`fields.view` exists only where a discriminator does.** Under form A the file is the view,
    // so there is nothing for a discriminator to select and a map naming one is a field the group
    // never declared — `configuration.md` §8's rule, made by [`KnownField::asserted_by`].
    let discriminator = || {
        KnownField::asserted_by(
            "view",
            !form_a,
            "under `[[view_group.view]]` the file is the view, so a row carries no discriminator \
             saying which view it lands in. Move the roster to `[view_group.views]` beside one \
             `source` if the points are in one file",
        )
    };
    let fields = if projection == Projection::None {
        compile_unprojected_fields(
            &object,
            source.as_ref(),
            block.fields.as_ref(),
            defaults,
            vec![discriminator()],
        )?
    } else {
        compile_projected_fields(
            &object,
            source.as_ref(),
            block.fields.as_ref(),
            defaults,
            vec![discriminator()],
        )?
    };

    let extent = compile_extent(&object, projection, block.extent.as_ref())?;
    let declared = block
        .visibility
        .clone()
        .map(tessera_types::view::DeclaredGate::into_labels);
    let visibility = compile_view_gate(&object, declared.as_deref())?;
    let point_visibility =
        compile_point_visibility(&object, block.point_visibility.as_ref(), sources)?;
    // The discriminator's column name, which no metadata name may take — `None` under form A,
    // where there is no discriminator to collide with.
    let carries_discriminator = (!form_a).then(|| fields.of("view").to_string());
    let metadata = compile_metadata_types(
        &object,
        block,
        vocabularies,
        carries_discriminator.as_deref(),
    )?;

    let roster = if form_a {
        let mut roster: Vec<RosterView> = Vec::with_capacity(block.view.len());
        for entry in &block.view {
            let view = compile_roster_view(&object, entry, &metadata, sources)?;
            if roster.iter().any(|v| v.key == view.key) {
                return Err(declaration_error(format!(
                    "{object}: view key '{}' is declared twice. A key is the caller's own name for \
                     one view, tombstoned on drop and never reused, and `<group>:<key>` is the id \
                     every request and every stored path is written under (views §3.2)",
                    view.key
                )));
            }
            roster.push(view);
        }
        Roster::Inline(roster)
    } else if let Some(table) = &block.views {
        Roster::Table(compile_roster_table(
            &object, table, &metadata, sources, defaults,
        )?)
    } else {
        // **A group declaring neither form carries no metadata**: its views are minted from the
        // discriminator's distinct values as the points are read, so there is no roster record for
        // a per-view value to sit on (views §3.1).
        if block.metadata.is_some() {
            return Err(declaration_error(format!(
                "{object}: `metadata` with no roster. The views here are minted from the \
                 discriminator's distinct values as the points are read, so there is no roster \
                 record for a per-view value to sit on (views §3.1). Write `[view_group.views]` — \
                 one row per view, carrying `key` and the metadata names — or drop the `metadata` \
                 line"
            )));
        }
        Roster::Discriminator
    };

    Ok(ViewGroup {
        name: block.name.clone(),
        title: block.title.clone(),
        projection,
        source,
        fields,
        extent,
        point_visibility,
        visibility,
        members,
        metadata,
        roster,
    })
}

/// A `[[view]]`'s or a `[[view_group]]`'s `point_visibility` (`configuration.md` §1).
///
/// One routine for both, because the key means exactly the same thing on each: a group's views
/// share their point-label acquisition as they share their frame, and a second copy of these four
/// refusals would be a second place for `inherited` to become a label.
fn compile_point_visibility(
    object: &str,
    block: Option<&PointVisibilityBlock>,
    sources: &Sources,
) -> Result<PointVisibility> {
    let point = block.ok_or_else(|| {
        declaration_error(format!(
            "{object}: `point_visibility` is required and has no default (configuration.md §1). \
             Write `point_visibility = {{ field = \"<column>\", default = \"<label>\" }}`: `field` \
             says where each point's own access label is, and `default` says what a point carrying \
             none gets — `public` reaches every principal, any other word is an access label, and \
             omitting it refuses such a point at both entry points (decision 0133). There is no \
             default because the value an absent line would supply is one of those, and each is a \
             decision"
        ))
    })?;
    if let Some(field) = &point.field {
        if field.trim().is_empty() {
            return Err(declaration_error(format!(
                "{object}: `point_visibility.field` is empty. Omit it to say points carry no \
                 labels of their own"
            )));
        }
    }
    // **A point's label comes from a field or from a source, never both** (§1). They are two
    // shapes of one relation — a list per point, or a row per `(point, term)` — so a declaration
    // naming both has said the labels are in two places and left the build to choose.
    if point.field.is_some() && point.source.is_some() {
        return Err(declaration_error(format!(
            "{object}: `point_visibility` declares both a `field` and a `source`, and a point's \
             label comes from one or the other (configuration.md §1). `field` is a column of this \
             view's own source, one value or a list per point; `source` is a separate exploded \
             `(entity_id, term_id)` relation. Declaring both leaves which one carries a point's \
             terms to the reader"
        )));
    }
    let labels = match &point.source {
        Some(declared) => Some(sources.path(&format!("{object} point_visibility"), declared)?),
        None => None,
    };
    // **`default` is optional, and its absence is the decision to refuse** (decision 0133): a
    // point carrying no label is then refused at the build and on `/control/ingest`, naming the
    // count. A declaration with no acquisition key and no default has no label for any point, so
    // it is refused here rather than at the first row.
    let Some(default) = point.default.as_deref() else {
        if point.field.is_none() && point.source.is_none() {
            return Err(declaration_error(format!(
                "{object}: `point_visibility` names no `field`, no `source` and no `default`, so \
                 no point has a label. Name where each point's own label is, or the label every \
                 point takes (configuration.md §1)"
            )));
        }
        return Ok(PointVisibility {
            field: point.field.clone(),
            source: labels,
            default: None,
        });
    };
    if default == INHERITED {
        return Err(declaration_error(format!(
            "{object}: `point_visibility.default = \"inherited\"` is refused. A container's gate \
             narrows rather than widens, and a point carrying no terms is already in no \
             principal's mask — so inheriting would have to *add* a term to the point, which can \
             only widen it (configuration.md §4). Name the label such a point should carry, or \
             `public`"
        )));
    }
    check_label(object, "point_visibility.default", default)?;
    Ok(PointVisibility {
        field: point.field.clone(),
        source: labels,
        default: Some(default.to_string()),
    })
}

/// The roster's own key set, which no metadata name may take (`views.md` §3.2).
const ROSTER_KEYS: [&str; 3] = ["key", "source", "visibility"];

/// The keys a group declares and a view of it may not (`views.md` §3.1).
const GROUP_LEVEL_KEYS: [&str; 8] = [
    "name",
    "title",
    "projection",
    "extent",
    "point_visibility",
    "metadata",
    "members",
    "fields",
];

/// The names and types a view of this group carries (`views.md` §3.1).
///
/// **Metadata names are bounded by the roster's own keys** (§3.2): `key`, `source`, `visibility`
/// and, where the group carries one, the discriminator's column name are refused as metadata
/// names. A `[[view_group.view]]` block mixes the closed set with the declared names, so a name in
/// both is a key with two readings — and on the roster table it would be one column asked to carry
/// two things.
fn compile_metadata_types(
    object: &str,
    block: &ViewGroupBlock,
    vocabularies: &HashMap<String, Vocabulary>,
    discriminator: Option<&str>,
) -> Result<Vec<ViewMetadata>> {
    let Some(declared) = &block.metadata else {
        return Ok(Vec::new());
    };
    let mut metadata = Vec::with_capacity(declared.len());
    for (name, value) in declared {
        check_metadata_name(object, name)?;
        if ROSTER_KEYS.contains(&name.as_str()) {
            return Err(declaration_error(format!(
                "{object}: `metadata.{name}` takes a name the roster already uses. `key`, `source` \
                 and `visibility` are the roster's own keys (views §3.2), and a \
                 `[[view_group.view]]` block mixes them with the metadata names — so a name in \
                 both is a key with two readings, on the block and as a column of \
                 `[view_group.views]`"
            )));
        }
        if discriminator == Some(name.as_str()) {
            return Err(declaration_error(format!(
                "{object}: `metadata.{name}` takes the name of this group's discriminator column, \
                 which `fields.view` puts at '{name}' (views §3.2). One column cannot both say \
                 which view a row lands in and carry a per-view value"
            )));
        }
        metadata.push(compile_metadata_type(object, name, value, vocabularies)?);
    }
    Ok(metadata)
}

/// A metadata name is a field name on the wire — `/v1/meta` serves it inside each roster entry —
/// so it takes the column charset, on [`check_column_name`]'s argument.
fn check_metadata_name(object: &str, name: &str) -> Result<()> {
    if name.trim().is_empty() {
        return Err(declaration_error(format!(
            "{object}: a metadata name is empty. It is the name a view's value is served under"
        )));
    }
    if !name
        .bytes()
        .all(|b| b.is_ascii_alphanumeric() || b == b'_' || b == b'-')
    {
        return Err(declaration_error(format!(
            "{object}: `metadata.{name}` is limited to ASCII letters, digits, `_` and `-`. It is a \
             field name on the wire — `/v1/meta` serves it inside the roster entry for each view — \
             on the same argument a column name is (contracts §3.2)"
        )));
    }
    Ok(())
}

/// One metadata declaration: `name = "<type>"`, or `name = { type = "category", vocabulary = … }`.
fn compile_metadata_type(
    object: &str,
    name: &str,
    value: &toml::Value,
    vocabularies: &HashMap<String, Vocabulary>,
) -> Result<ViewMetadata> {
    let (ty_name, vocabulary) = match value {
        toml::Value::String(word) => (word.as_str(), None),
        toml::Value::Table(table) => {
            for key in table.keys() {
                if key != "type" && key != "vocabulary" {
                    return Err(declaration_error(format!(
                        "{object}: `metadata.{name}.{key}` is not a key of a metadata \
                         declaration, which takes `type` and — for a category — `vocabulary` \
                         (views §3.1). A metadata value is one value per view on the roster, so it \
                         carries none of an attribute's placement or index keys: it filters \
                         nothing and occupies no column"
                    )));
                }
            }
            let ty = table
                .get("type")
                .and_then(toml::Value::as_str)
                .ok_or_else(|| {
                    declaration_error(format!(
                        "{object}: `metadata.{name}` is a table with no `type`. The table spelling \
                         is for a category — `{{ type = \"category\", vocabulary = \"<name>\" }}` \
                         — and every other type is the word alone (views §3.1)"
                    ))
                })?;
            (ty, table.get("vocabulary").and_then(toml::Value::as_str))
        }
        other => {
            return Err(declaration_error(format!(
                "{object}: `metadata.{name}` is {}, and a metadata declaration is a type name — \
                 one of the `[[attribute]]` types — or `{{ type = \"category\", vocabulary = … }}` \
                 (views §3.1)",
                other.type_str()
            )))
        }
    };
    if ty_name == "category" {
        let vocabulary = vocabulary.ok_or_else(|| {
            declaration_error(format!(
                "{object}: `metadata.{name}` is a category and names no `vocabulary`. It names a \
                 `[[vocabulary]]` block, which is where the width, the value set and the \
                 visibility live — every one of them a decision nobody can make on the author's \
                 behalf"
            ))
        })?;
        let declared = vocabularies.get(vocabulary).ok_or_else(|| {
            declaration_error(format!(
                "{object}: `metadata.{name}` names vocabulary '{vocabulary}', which no \
                 `[[vocabulary]]` block declares. Declared: {}. A missing block is refused rather \
                 than minted as an open vocabulary — a typo would otherwise create a value set \
                 nobody authored, at whatever width and visibility the fall-through picked",
                declared_names(vocabularies)
            ))
        })?;
        return Ok(ViewMetadata {
            name: name.to_string(),
            ty: declared.width,
            vocabulary: Some(declared.name.clone()),
        });
    }
    if vocabulary.is_some() {
        return Err(declaration_error(format!(
            "{object}: `metadata.{name}` is type '{ty_name}', not a category, so `vocabulary` has \
             no meaning for it. Refused rather than ignored: a value set on a value that has none \
             is a control its author believes is set"
        )));
    }
    let ty = ScalarType::parse(ty_name)
        .filter(|ty| *ty != ScalarType::Utf8)
        .ok_or_else(|| {
            declaration_error(format!(
                "{object}: `metadata.{name}` declares unknown type '{ty_name}'. The types are the \
                 `[[attribute]]` types: bool, u8, u16, u32, u64, i8, i16, i32, i64, f32, f64, \
                 timestamp_us, keyword, text and category"
            ))
        })?;
    Ok(ViewMetadata {
        name: name.to_string(),
        ty,
        vocabulary: None,
    })
}

/// One `[[view_group.view]]` block — form A's roster record (`views.md` §3.1).
///
/// **Parsed by hand, and the closure is kept by hand with it.** The block mixes a closed key set
/// with the group's declared metadata names, so no derive knows its field list and
/// `deny_unknown_fields` cannot be what refuses an unknown key. This is the manual route the
/// `extent` spellings already take, and the guarantee `configuration.md` §1 rests on — an unknown
/// key is refused — is preserved by checking against the closed set *plus* the declared names.
fn compile_roster_view(
    group: &str,
    entry: &toml::Value,
    metadata: &[ViewMetadata],
    sources: &Sources,
) -> Result<RosterView> {
    let table = entry.as_table().ok_or_else(|| {
        declaration_error(format!(
            "{group}: a `[[view_group.view]]` entry is {}, and one view is a table of `key`, \
             `source`, `visibility` and this group's metadata names (views §3.1)",
            entry.type_str()
        ))
    })?;
    let key = table
        .get("key")
        .and_then(toml::Value::as_str)
        .ok_or_else(|| {
            declaration_error(format!(
                "{group}: a `[[view_group.view]]` block declares no `key`. The key is the caller's \
                 own name for the view and is required at creation — `<group>:<key>` is the id \
                 every request names and a view's only address (views §3.2)"
            ))
        })?;
    let object = format!("{group}, view '{key}'");
    check_view_name(&object, key)?;

    for name in table.keys() {
        if ROSTER_KEYS.contains(&name.as_str()) || metadata.iter().any(|m| &m.name == name) {
            continue;
        }
        // **The group-level keys, named as such.** Each is a setting a group's views share by
        // definition — one frame, one projection, one point-label rule — so a caller who wrote one
        // on a view has not mistyped a key: they have asked for a per-view setting the design does
        // not have, and the message says so rather than reporting an unknown key.
        if GROUP_LEVEL_KEYS.contains(&name.as_str()) {
            return Err(declaration_error(format!(
                "{object}: `{name}` is a group-level key and a view of a group may not declare it \
                 (views §3.1). A group's views share every setting — projection, extent, point \
                 visibility, gate — and differ only by a key and per-view metadata; a view needing \
                 its own frame is a second `[[view_group]]`, or a plain `[[view]]`. Write `{name}` \
                 on the group"
            )));
        }
        return Err(declaration_error(format!(
            "{object}: `{name}` is not a key of a `[[view_group.view]]` block. It takes `key`, \
             `source`, `visibility` and one key per declared metadata name — {}. The key set is \
             closed exactly as every other block's is (configuration.md §1); a name this group \
             declares no metadata under is a typed per-view value nobody declared the type of",
            match metadata.len() {
                0 => "and this group declares no metadata".to_string(),
                _ => format!("here: {}", names(metadata.iter().map(|m| m.name.as_str()))),
            }
        )));
    }

    let source = match table.get("source") {
        None => None,
        Some(value) => {
            let declared = value.as_str().ok_or_else(|| {
                declaration_error(format!(
                    "{object}: `source` is {}, and it names a key of `[sources]`",
                    value.type_str()
                ))
            })?;
            Some(sources.path(&object, declared)?)
        }
    };
    let visibility = match table.get("visibility") {
        None => None,
        Some(value) => {
            let declared: tessera_types::view::DeclaredGate =
                value.clone().try_into().map_err(|_| {
                    declaration_error(format!(
                        "{object}: `visibility` is {}, and it is an access label, a list of \
                         access labels, or `public` (views §6)",
                        value.type_str()
                    ))
                })?;
            compile_view_gate(&object, Some(&declared.into_labels()))?
        }
    };

    // **Every declared name, on every view.** A roster record is immutable (decision 0108): a
    // value left out is not filled in later by an update, it is a view served with a typed field
    // missing for the whole of its life. The declaration is what an author can still change, so
    // the absence is refused here rather than served as a hole.
    let mut values = BTreeMap::new();
    for declared in metadata {
        let value = table.get(&declared.name).ok_or_else(|| {
            declaration_error(format!(
                "{object}: no `{}`, which this group declares as metadata every view carries \
                 (views §3.1). A roster record is immutable (decision 0108), so a value left out \
                 is a view served with that field missing for the whole of its life rather than \
                 one an update fills in later",
                declared.name
            ))
        })?;
        values.insert(
            declared.name.clone(),
            compile_metadata_value(&object, declared, value)?,
        );
    }

    Ok(RosterView {
        key: key.to_string(),
        source,
        visibility,
        metadata: values,
    })
}

/// One metadata value on one roster record, typed against its declaration.
///
/// **Typed at the declaration and not at the reader**, because the roster is served on `/v1/meta`
/// as typed values (`views.md` §3.2): a `starts` written as a string in one block and as a
/// date-time in the next is one field with two wire types, and the client reading it has no way to
/// know which it will get.
fn compile_metadata_value(
    object: &str,
    declared: &ViewMetadata,
    value: &toml::Value,
) -> Result<MetadataValue> {
    let name = &declared.name;
    let wrong = |wanted: &str| {
        declaration_error(format!(
            "{object}: `{name}` is {}, and this group declares it '{}' — {wanted}",
            value.type_str(),
            declared.ty.arrow_type_name()
        ))
    };
    if let Some(vocabulary) = &declared.vocabulary {
        let key = value.as_str().ok_or_else(|| {
            declaration_error(format!(
                "{object}: `{name}` is {}, and this group declares it a category over vocabulary \
                 '{vocabulary}' — write the value's key as a string",
                value.type_str()
            ))
        })?;
        // The key is checked against the vocabulary at the build that reads it, exactly as a
        // category column's values are: an open vocabulary mints, and a closed one refuses, and
        // neither is a decision this parse can make for it.
        return Ok(MetadataValue::Text(key.to_string()));
    }
    Ok(match declared.ty {
        ScalarType::Bool => MetadataValue::Bool(
            value
                .as_bool()
                .ok_or_else(|| wrong("write `true` or `false`"))?,
        ),
        ScalarType::F32 | ScalarType::F64 => match value {
            toml::Value::Float(f) => MetadataValue::Float(*f),
            // An integer where a float is declared is the value the author wrote rather than a
            // type error: TOML spells `0` as an integer and there is one reading of it here.
            toml::Value::Integer(i) => MetadataValue::Float(*i as f64),
            _ => return Err(wrong("write a number")),
        },
        ScalarType::TimestampUs => match value {
            toml::Value::Datetime(when) => {
                MetadataValue::TimestampUs(timestamp_us(object, name, when)?)
            }
            // The stored representation, for a producer that emits it directly.
            toml::Value::Integer(us) => MetadataValue::TimestampUs(*us),
            _ => return Err(wrong(
                "write an offset date-time (`2026-04-01T00:00:00Z`), or the microseconds since \
                     the Unix epoch as an integer",
            )),
        },
        ScalarType::Utf8 | ScalarType::Keyword | ScalarType::Text => MetadataValue::Text(
            value
                .as_str()
                .ok_or_else(|| wrong("write a string"))?
                .to_string(),
        ),
        integer => {
            let held = value
                .as_integer()
                .ok_or_else(|| wrong("write an integer"))?;
            let (min, max) = integer_range(integer);
            if held < min || held > max {
                return Err(declaration_error(format!(
                    "{object}: `{name}` is {held}, and this group declares it '{}', which holds \
                     {min} to {max}. The width is part of the declaration, so the value is refused \
                     rather than narrowed",
                    integer.arrow_type_name()
                )));
            }
            MetadataValue::Int(held)
        }
    })
}

/// A TOML date-time as microseconds since the Unix epoch.
///
/// **An offset is required.** A local date-time names an instant only against a time zone nobody
/// declared, and a `timestamp_us` is a fixed point on the line — so a local one is refused rather
/// than read as UTC, which would move a quarter boundary silently by up to a day.
fn timestamp_us(object: &str, name: &str, when: &toml::value::Datetime) -> Result<i64> {
    let text = when.to_string();
    let parsed = chrono::DateTime::parse_from_rfc3339(&text).map_err(|_| {
        declaration_error(format!(
            "{object}: `{name} = {text}` is not an instant this can store. A `timestamp_us` is \
             microseconds since the Unix epoch, so the value needs a date, a time and an offset — \
             `2026-04-01T00:00:00Z`. A local date-time names an instant only against a time zone \
             nobody declared here"
        ))
    })?;
    Ok(parsed.timestamp_micros())
}

/// The inclusive range an integer type holds, for a metadata value to be checked against.
fn integer_range(ty: ScalarType) -> (i64, i64) {
    match ty {
        ScalarType::U8 => (0, u8::MAX as i64),
        ScalarType::U16 => (0, u16::MAX as i64),
        ScalarType::U32 => (0, u32::MAX as i64),
        // `u64`'s upper half is not expressible in TOML's own signed integer, which is where this
        // value is read from — so the ceiling is the reader's, stated rather than silently wrapped.
        ScalarType::U64 => (0, i64::MAX),
        ScalarType::I8 => (i8::MIN as i64, i8::MAX as i64),
        ScalarType::I16 => (i16::MIN as i64, i16::MAX as i64),
        ScalarType::I32 => (i32::MIN as i64, i32::MAX as i64),
        _ => (i64::MIN, i64::MAX),
    }
}

/// `[view_group.views]` — form B's roster table (`views.md` §3.1).
///
/// **The roster's own file, not the group's.** The group's `source` holds the points, one row per
/// `(entity, view)`; this holds one row per view — the canonical `key`, each view's `visibility`
/// and one column per declared metadata name, each defaulting to its own name.
fn compile_roster_table(
    group: &str,
    table: &RosterTableBlock,
    metadata: &[ViewMetadata],
    sources: &Sources,
    defaults: &Defaults,
) -> Result<RosterTable> {
    let object = format!("{group} `[view_group.views]`");
    let source = table.source.as_ref().ok_or_else(|| {
        declaration_error(format!(
            "{object}: `source` is required. The table is one row per view — the canonical `key`, \
             `visibility` and this group's metadata names — and it is the roster's own file, \
             separate from the group's `source`, which holds the points (views §3.1)"
        ))
    })?;
    let path = sources.path(&object, source)?;
    let mut known = vec![KnownField::always("key"), KnownField::always("visibility")];
    known.extend(
        metadata
            .iter()
            .map(|declared| KnownField::always(declared.name.clone())),
    );
    let fields = check_fields(
        &object,
        Some(&path),
        &known,
        table.fields.as_ref(),
        &defaults.entity_id_field,
    )?;
    Ok(RosterTable {
        source: path,
        fields,
    })
}

/// `scope` on an `[[attribute]]` or a `[[layer]]` (`views.md` §5, §3.5; decision 0109).
///
/// **Two spellings, matched by hand.** `"entity"` is the default and declares nothing — a constant
/// attribute is one value per entity, evaluated in entity space, and therefore visible under every
/// view. `{ group = "…" }` is the case that needs declaring: a value, or an artifact set, that
/// differs by view of a group. An untagged enum over the word and the table would report a
/// mistyped key inside the table as *no variant matched*, which is the same reason `membership`
/// and `extent` are held as `toml::Value`.
fn compile_scope(object: &str, value: Option<&toml::Value>, groups: &[ViewGroup]) -> Result<Scope> {
    let Some(value) = value else {
        return Ok(Scope::Entity);
    };
    let named = match value {
        toml::Value::String(word) if word == "entity" => return Ok(Scope::Entity),
        toml::Value::String(word) => {
            return Err(declaration_error(format!(
                "{object}: `scope = \"{word}\"` is not a value this key takes. The two spellings \
                 are `scope = \"entity\"` — the default, one value per entity under every view — \
                 and `scope = {{ group = \"<view_group>\" }}`, one per view of that group \
                 (views §5)"
            )))
        }
        toml::Value::Table(table) => {
            for key in table.keys() {
                if key != "group" {
                    return Err(declaration_error(format!(
                        "{object}: `scope.{key}` is not a key of a scope, which takes `group` \
                         alone (views §5). A scope names what the value varies by, and the one \
                         thing it may vary by is a view group"
                    )));
                }
            }
            table
                .get("group")
                .and_then(toml::Value::as_str)
                .ok_or_else(|| {
                    declaration_error(format!(
                        "{object}: `scope` is a table with no `group`. Write \
                         `scope = {{ group = \"<view_group>\" }}` to say the value differs by view \
                         of that group, or `scope = \"entity\"` — the default — for one value per \
                         entity (views §5)"
                    ))
                })?
        }
        other => {
            return Err(declaration_error(format!(
                "{object}: `scope` is {}, and it is either the word `entity` or the table \
                 `{{ group = \"<view_group>\" }}` (views §5)",
                other.type_str()
            )))
        }
    };
    let group = groups.iter().find(|g| g.name == named).ok_or_else(|| {
        declaration_error(format!(
            "{object}: `scope = {{ group = \"{named}\" }}` names no `[[view_group]]` block. \
             Declared: {}. A scope binds a value to a group's views, so a name nothing declares is \
             a column family with no columns in it",
            names(groups.iter().map(|g| g.name.as_str()))
        ))
    })?;
    // **The group named is the one that owns the views** (§5). A `members` group's keys are its
    // owner's, so a scope on it would be a second name for one column family — and the two would
    // then have to agree about a value neither owns. The refusal points at the owner, where the
    // declaration belongs and from where it reaches this group anyway.
    if let Some(owner) = &group.members {
        return Err(declaration_error(format!(
            "{object}: `scope = {{ group = \"{named}\" }}` names a group that declares \
             `members = \"{owner}\"`, so its views are '{owner}'s (views §5). Scope on the group \
             that owns the views — `scope = {{ group = \"{owner}\" }}` — which applies to every \
             group sharing them, this one included"
        )));
    }
    Ok(Scope::Group(named.to_string()))
}

/// Every group that draws on `group`'s key set: the group itself, and every group declaring
/// `members = "<group>"` (`views.md` §3.3).
fn groups_sharing<'a>(groups: &'a [ViewGroup], group: &str) -> Vec<&'a str> {
    groups
        .iter()
        .filter(|g| g.name == group || g.members.as_deref() == Some(group))
        .map(|g| g.name.as_str())
        .collect()
}

/// Which attributes are bound to a group's views, by name (`views.md` §5).
fn compile_attribute_scopes(
    blocks: &[AttributeBlock],
    groups: &[ViewGroup],
) -> Result<BTreeMap<String, String>> {
    let mut scopes = BTreeMap::new();
    for block in blocks {
        let object = format!("attribute '{}'", block.name);
        // Only the scoped ones are recorded: entity scope is the default and absence says it,
        // so nothing has to be written to declare the ordinary thing ([`Scopes`]).
        if let Scope::Group(group) = compile_scope(&object, block.scope.as_ref(), groups)? {
            scopes.insert(block.name.clone(), group);
        }
    }
    Ok(scopes)
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
    scopes: &BTreeMap<String, String>,
    sources: &Sources,
    defaults: &Defaults,
) -> Result<(Vec<Attribute>, Vec<ScopedAttribute>)> {
    let mut attributes = Vec::with_capacity(blocks.len());
    let mut scoped: Vec<ScopedAttribute> = Vec::new();
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
                 expression names columns directly, so a column may not take one. Reserved: {}",
                decl.name,
                RESERVED_COLUMN_NAMES.join(", ")
            )));
        }
        // The two reserved *leaves*, on the same argument: `region` is the spatial one
        // (selection-operand §2) and `member_of` names one artifact's membership
        // (`highlight-and-hierarchy.md` §3). A column of either name would make a request mean two
        // things.
        if matches!(decl.name.as_str(), "region" | "member_of") {
            return Err(declaration_error(format!(
                "attribute '{}': that name is a filter leaf of the request surface \
                 (`selection-operand.md` §2, `highlight-and-hierarchy.md` §3), and a filter \
                 expression names columns directly, so a column may not take it. Reserved: {}",
                decl.name,
                RESERVED_COLUMN_NAMES.join(", ")
            )));
        }
        // **The frames' own reserved name.** `highlighted` is a column of the *tiles*, *points* and
        // *artifacts* frames (`highlight-and-hierarchy.md` §2), so a render column of that name
        // would put two columns of one name on the points frame and a by-name reader would take
        // the wrong one.
        if decl.name == "highlighted" {
            return Err(declaration_error(format!(
                "attribute '{}': that name is the *points* frame's highlight column \
                 (`highlight-and-hierarchy.md` §2), so a render column of it would put two \
                 columns of one name on one frame and a by-name reader would take the wrong one. \
                 Reserved: {}",
                decl.name,
                RESERVED_COLUMN_NAMES.join(", ")
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
        // **A group-scoped column is not one of `MANIFEST.declared_scalars`** (`views.md` §5):
        // it is a *family* — one entity-space column per view of the group — and the manifest's
        // list is one flat set of bundle-wide columns. Held apart here rather than filtered at
        // each consumer, so no pass can forget: a scoped column in the schema would take a slot
        // in every row's hot tail and a whole-corpus `attrs/<column>/` of its own, both of them
        // absent for every entity, and both served as if the attribute were entity-scoped.
        match scopes.get(&decl.name) {
            None => {
                // **`fields` names the view discriminator and nothing else**, so an entity-scoped
                // column has nothing to say with it: its values are one per entity and no column
                // of its file decides which view they are for. Refused rather than ignored, on
                // this module's rule for every disclosure-adjacent key — a `fields` map that reads
                // as a default is a routing its author believes is in effect.
                if decl.fields.is_some() {
                    return Err(declaration_error(format!(
                        "attribute '{}': `fields` names the view discriminator on a group-scoped \
                         column's own source (views §5), and this column is entity scope — one \
                         value per entity, under every view — so there is nothing for it to \
                         choose between. The entity id is `entity_id_field`",
                        decl.name
                    )));
                }
                attributes.push(attribute)
            }
            Some(group) => {
                // **A scoped attribute's own `source` carries the discriminator** (`views.md`
                // §5): one row per `(entity, view)`, `fields.view` saying which view each row's
                // value is for, and the row routed to that view's column. Without the
                // discriminator the file would be read as entity space, which would take one
                // arbitrary view's values as every view's — so the column is resolved here, where
                // the source name and the field spellings are both in hand.
                // **A scoped `text` column must be indexed, because the index is its only
                // home.** An entity-scoped text column has two — a token index answering `match`
                // and a record-blob row answering `entity → value` — and the blob is bundle-wide,
                // addressed by a column's position in `declared_scalars`, which a family has none
                // of. So an unindexed scoped text column would be a declared field stored nowhere
                // at all: acknowledged and then lost. Refused rather than reported, on the rule
                // that separates a config a build can honour from one it cannot.
                if attribute.ty == ScalarType::Text && !attribute.index {
                    return Err(declaration_error(format!(
                        "attribute '{}': a `text` column scoped to group '{group}' needs \
                         `index = true`. Its terms are its only home — the record blob is one \
                         bundle-wide list with no slot for a column family (views §5) — so \
                         without the index the prose would be read and stored nowhere",
                        decl.name
                    )));
                }
                let source = compile_scoped_attribute_source(decl, sources, defaults)?;
                scoped.push(ScopedAttribute {
                    attribute,
                    group: group.clone(),
                    source,
                });
            }
        }
    }
    Ok((attributes, scoped))
}

/// A group-scoped attribute's own source, where it declares one (`views.md` §5).
///
/// **`[defaults].source` does not reach here, and that is deliberate.** The default is a single
/// whole-corpus file with one row per entity, which is exactly the wrong shape: a scoped column's
/// values are one per `(entity, view)`. So a scoped column reads from each view's own points file
/// unless it names a `source` of its own, and naming one is a statement that *this* file carries
/// the discriminator.
///
/// The discriminator column is `fields.view`, defaulting to `view` — the same key and the same
/// default a scoped layer's artifacts source takes, so one word means one thing across the
/// declaration. Every other key in the map is refused: `view` is the only field this source
/// resolves, the entity id being `entity_id_field` beside it.
fn compile_scoped_attribute_source(
    decl: &AttributeBlock,
    sources: &Sources,
    defaults: &Defaults,
) -> Result<Option<ScopedAttributeFile>> {
    let object = format!("attribute '{}'", decl.name);
    let Some(name) = decl.source.as_deref() else {
        if decl.fields.is_some() {
            return Err(declaration_error(format!(
                "{object}: `fields` names the view discriminator on this column's own `source` \
                 (views §5), and there is no `source` here — the column is read from each view's \
                 own points file, where the view is the file rather than a column of it"
            )));
        }
        return Ok(None);
    };
    let path = sources.path(&object, name)?;
    let entity_id = match decl.entity_id_field.as_deref() {
        None => defaults.entity_id_field.clone(),
        Some(field) if field.trim().is_empty() => {
            return Err(declaration_error(format!(
                "{object}: `entity_id_field` is empty, so it names no column. Omit it to join on \
                 '{}', which is what this declaration spells the entity id",
                defaults.entity_id_field
            )))
        }
        Some(field) => field.to_string(),
    };
    let mut view_field = "view".to_string();
    for (key, value) in decl.fields.iter().flatten() {
        if key != "view" {
            return Err(declaration_error(format!(
                "{object}: `fields.{key}` is not a field of a group-scoped attribute's source, \
                 which resolves `view` alone — the column saying which view each row's value is \
                 for. The entity id is `entity_id_field`"
            )));
        }
        if value.trim().is_empty() {
            return Err(declaration_error(format!(
                "{object}: `fields.view` is empty, so it names no column. Omit it to read the \
                 discriminator from 'view'"
            )));
        }
        view_field = value.clone();
    }
    Ok(Some(ScopedAttributeFile {
        path,
        entity_id,
        view_field,
    }))
}

fn declared_names(vocabularies: &HashMap<String, Vocabulary>) -> String {
    if vocabularies.is_empty() {
        return "none".to_string();
    }
    let mut names: Vec<&str> = vocabularies.keys().map(String::as_str).collect();
    names.sort_unstable();
    names.join(", ")
}

/// The names an attribute may not take, in the order the refusals list them.
///
/// **One list, read by the three refusals above**, so a name added to the request surface is added
/// here and every message says the same set. `all_of`/`any_of`/`none_of` are the combinators
/// (decision 0062); `region` is the spatial leaf (`selection-operand.md` §2); `member_of` names
/// one artifact's membership (`highlight-and-hierarchy.md` §3). A filter expression names columns
/// directly — there is no wrapper object — so a column of any of these names would make a request
/// mean two things. `highlighted` is not a leaf but a **frame column**
/// (`highlight-and-hierarchy.md` §2): a render column of that name would put two columns of one
/// name on the points frame, and a by-name reader would take the wrong one.
pub const RESERVED_COLUMN_NAMES: [&str; 6] = [
    "all_of",
    "any_of",
    "none_of",
    "region",
    "member_of",
    "highlighted",
];

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
/// which layers the `[layer.labels]` sugar wrote, by parent, and which layers are scoped to a view
/// group — four parallel views of one pass, kept apart because a [`LayerDeclaration`] is exactly
/// the control-plane payload and must carry none of the others.
type CompiledLayers = (
    Vec<LayerDeclaration>,
    Vec<LayerSources>,
    BTreeMap<String, String>,
    BTreeMap<String, String>,
);

fn compile_layers(
    declared: &[LayerBlock],
    views: &[View],
    groups: &[ViewGroup],
    attributes: &[Attribute],
    sources: &Sources,
) -> Result<CompiledLayers> {
    // Sugar first, so nothing below this line knows a label layer from a layer.
    let (blocks, from_labels) = expand_labels(declared)?;
    let mut layers = Vec::with_capacity(blocks.len());
    let mut per_layer = Vec::with_capacity(blocks.len());
    let mut scopes: BTreeMap<String, String> = BTreeMap::new();
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
        // **A name in `views` is a plain view or a whole group** (`views.md` §2): naming a group
        // draws the layer on every view of it, present and future, which is what lets a layer
        // follow a group that grows at ingest rather than being redeclared per quarter.
        for view in declared_views {
            if !views.iter().any(|v| &v.name == view) && !groups.iter().any(|g| &g.name == view) {
                return Err(declaration_error(format!(
                    "layer '{}' declares view '{view}', which no `[[view]]` or `[[view_group]]` \
                     block declares. Declared: {}. A layer in a view that does not exist is \
                     registered, reachable and empty, which no client can tell from one whose \
                     artifacts were all withheld",
                    block.name,
                    names(
                        views
                            .iter()
                            .map(|v| v.name.as_str())
                            .chain(groups.iter().map(|g| g.name.as_str()))
                    )
                )));
            }
        }

        // **A scoped layer is a different artifact set per view of one group** (`views.md` §3.5),
        // so the views it is drawn on can only be that group's: an artifact belongs to one view,
        // and a plain view is not one of them. The groups sharing the key set are admitted with
        // it, since their views *are* the same views.
        let scope = compile_scope(&object, block.scope.as_ref(), groups)?;
        if let Scope::Group(group) = &scope {
            let sharing = groups_sharing(groups, group);
            for view in declared_views {
                if !sharing.contains(&view.as_str()) {
                    return Err(declaration_error(format!(
                        "{object}: `scope = {{ group = \"{group}\" }}` with `views` naming \
                         '{view}'. A scoped layer's artifacts belong to one view each and are \
                         keyed per `(layer, view)`, so the views it is drawn on are that group's \
                         and no others (views §3.5). Nameable here: {}. Drop the scope for one \
                         artifact set drawn on every view named, which is the default",
                        names(sharing.iter().copied())
                    )));
                }
            }
        }

        let membership = compile_membership(block, attributes)?;

        let hierarchy = compile_hierarchy(block)?;
        let visibility = match block.visibility.as_deref() {
            Some(PUBLIC) => None,
            Some(label) => {
                check_label(&object, "visibility", label)?;
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
                check_label(&object, "artifact_visibility.default", default)?;
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
                 every computed property is recomputed from what is left. There is no key of this \
                 name on `[layer.content]`: a deleted member withdraws supplied content at the fold \
                 (decision 0135)",
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
        let shape = compile_shape(block, &membership)?;
        let shape_kind = shape.map(|s| s.kind);
        let kind_is = |kind: ShapeKind| shape_kind == Some(kind);
        // **A layer has geometry to be in a space if it declares either kind** — a membership
        // shape in `[layer.shape]`, or an authored shape content, which is read in the space its
        // row declares exactly as a membership shape is (`polygon-membership.md` §6.1). The two
        // are never declared together, so at most one of them is what a space governs.
        let carries_geometry = shape.is_some()
            || content
                .supplied
                .iter()
                .any(|s| s.authored_shape_kind().is_some());
        // **A row's geometry sits in its layer's kind's fields and no other** — the box's four
        // bounds, the circle's three, the ellipse's five, the polygon's WKB `geometry` column
        // (GeoParquet's own name) — so naming a field of another kind is refused as a field the
        // layer never declared.
        // **A shape layer's views need share neither projection nor frame**
        // ([decision 0111](../../../docs/decisions/0111-a-shape-spans-projected-views-through-wgs84.md),
        // superseding the 2026-08-29 refusal that stood here): a `wgs84` shape is densified,
        // projected and quantised through each view's own declaration, once per view. What is
        // refused is the *mix* of a projected view and a `projection = "none"` one — `wgs84` means
        // nothing in an embedding — and that check is `check_shape_span`'s, applied at the build's
        // layer read and at `PUT /control/layers` alike, so there is one implementation of it. A
        // `view`-space row over unequal frames is refused too, where the row's own space is known.
        if carries_geometry {
            let mut named: Vec<(&str, Projection)> = Vec::new();
            for name in declared_views {
                if let Some(view) = views.iter().find(|v| &v.name == name) {
                    named.push((name.as_str(), view.projection));
                }
            }
            let projected = |p: &Projection| *p != Projection::None;
            if let (Some((a, _)), Some((b, _))) = (
                named.iter().find(|(_, p)| projected(p)).copied(),
                named.iter().find(|(_, p)| !projected(p)).copied(),
            ) {
                return Err(declaration_error(format!(
                    "{object}: view '{a}' declares a projection and view '{b}' declares \
                     `projection = \"none\"`. A `wgs84` coordinate means nothing in an \
                     embedding, so no geometry spans the two kinds of space (decision 0111); \
                     draw the layer on one kind or the other"
                )));
            }
        }

        // **A space is honourable only if the views the layer is drawn in can honour it**
        // (`polygon-membership.md` §4.3): `wgs84` asks the view to project, and a view declaring
        // `projection = "none"` has one space and nothing to convert a degree from. Refused here,
        // where the declaration can be pointed at, rather than at the first row read.
        let honourable =
            |space: tessera_store::derived::ShapeSpace| -> std::result::Result<(), String> {
                for name in declared_views {
                    let Some(view) = views.iter().find(|v| &v.name == name) else {
                        continue;
                    };
                    space.resolve(view.projection).map_err(|e| {
                        format!(
                            "view '{name}' declares `projection = \"{}\"`: {e}",
                            view.projection.name()
                        )
                    })?;
                }
                Ok(())
            };
        let default_space = match block.default_space.as_deref() {
            None => tessera_store::derived::ShapeSpace::View,
            Some(word) => {
                if !carries_geometry {
                    return Err(declaration_error(format!(
                        "{object}: `default_space` is declared and the layer declares neither \
                         `[layer.shape]` nor an authored shape content, so there is no geometry \
                         for it to be the space of"
                    )));
                }
                let space = tessera_store::derived::ShapeSpace::parse(word)
                    .map_err(|e| declaration_error(format!("{object}: `default_space`: {e}")))?;
                honourable(space)
                    .map_err(|e| declaration_error(format!("{object}: `default_space`: {e}")))?;
                space
            }
        };
        for artifact in block.artifacts.iter().flatten() {
            if let Some(word) = artifact.space.as_deref() {
                let space = tessera_store::derived::ShapeSpace::parse(word).map_err(|e| {
                    declaration_error(format!(
                        "{object}: artifact '{}': `space`: {e}",
                        artifact.key
                    ))
                })?;
                honourable(space).map_err(|e| {
                    declaration_error(format!(
                        "{object}: artifact '{}': `space`: {e}",
                        artifact.key
                    ))
                })?;
            }
        }
        let artifact_fields = check_fields(
            &object,
            source.as_ref(),
            &[
                KnownField::always("key"),
                KnownField::asserted_by(
                    "min_x",
                    kind_is(ShapeKind::Bbox),
                    "`[layer.shape].kind` is not \"bbox\"",
                ),
                KnownField::asserted_by(
                    "min_y",
                    kind_is(ShapeKind::Bbox),
                    "`[layer.shape].kind` is not \"bbox\"",
                ),
                KnownField::asserted_by(
                    "max_x",
                    kind_is(ShapeKind::Bbox),
                    "`[layer.shape].kind` is not \"bbox\"",
                ),
                KnownField::asserted_by(
                    "max_y",
                    kind_is(ShapeKind::Bbox),
                    "`[layer.shape].kind` is not \"bbox\"",
                ),
                KnownField::asserted_by(
                    "cx",
                    kind_is(ShapeKind::Circle) || kind_is(ShapeKind::Ellipse),
                    "`[layer.shape].kind` is neither \"circle\" nor \"ellipse\"",
                ),
                KnownField::asserted_by(
                    "cy",
                    kind_is(ShapeKind::Circle) || kind_is(ShapeKind::Ellipse),
                    "`[layer.shape].kind` is neither \"circle\" nor \"ellipse\"",
                ),
                KnownField::asserted_by(
                    "r",
                    kind_is(ShapeKind::Circle),
                    "`[layer.shape].kind` is not \"circle\"",
                ),
                KnownField::asserted_by(
                    "a",
                    kind_is(ShapeKind::Ellipse),
                    "`[layer.shape].kind` is not \"ellipse\"",
                ),
                KnownField::asserted_by(
                    "b",
                    kind_is(ShapeKind::Ellipse),
                    "`[layer.shape].kind` is not \"ellipse\"",
                ),
                KnownField::asserted_by(
                    "angle",
                    kind_is(ShapeKind::Ellipse),
                    "`[layer.shape].kind` is not \"ellipse\"",
                ),
                KnownField::asserted_by(
                    "geometry",
                    kind_is(ShapeKind::Polygon),
                    "`[layer.shape].kind` is not \"polygon\"",
                ),
                KnownField::asserted_by(
                    "view",
                    matches!(scope, Scope::Group(_)),
                    "the layer is entity-scoped — one artifact set drawn on every view it names \
                     — so no row says which view an artifact belongs to. `scope = { group = … }` \
                     is what declares a set per view (views §3.5)",
                ),
                KnownField::asserted_by(
                    "space",
                    carries_geometry,
                    "the layer declares neither `[layer.shape]` nor an authored shape content, so \
                     its rows carry no geometry to be in a space",
                ),
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
                        HierarchyKind::Nested | HierarchyKind::Dag | HierarchyKind::Tiered
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
                    default_space,
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

        let declaration = LayerDeclaration {
            // **The scope reaches the manifest on the declaration** (contracts §2.3): it was
            // compiled into `Scopes` alone, which is a build-time structure, so a bundle carried
            // no record of which of its layers were per-view (`views.md` §11).
            scope: match &scope {
                Scope::Entity => tessera_types::layer::LayerScope::Entity,
                Scope::Group(group) => tessera_types::layer::LayerScope::Group(group.clone()),
            },
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
        if let Scope::Group(group) = &scope {
            scopes.insert(block.name.clone(), group.clone());
        }
        layers.push(declaration);
    }
    Ok((layers, per_layer, from_labels, scopes))
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
            let Some(attribute) = attributes.iter().find(|a| a.name == field) else {
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
            };
            // **The membership *is* the column, so the column has to be one that partitions**
            // (`design/artifact-serving-at-scale.md` §5.1). Every point carries exactly one value
            // of a single-valued category-width column, which is what makes one label per row the
            // whole membership. The three refusals below are the three ways that stops being true,
            // and each is ⊘ scope rather than a defect:
            //
            // - **not indexed**: the values live in the render table or nowhere, and the predicate
            //   reads the entity-addressed `ValueColumn` an `index = true` column writes;
            // - **not a category width**: a `u64`, a float or a string has no code a label column
            //   can hold, and `ValueColumn::value_of` answers `u32::MAX` for one rather than the
            //   value — a membership every artifact would share;
            // - **`keyword` or `text`**: their ordinals are per *layer* of the index, so merging
            //   them across a base and its extents needs each layer's own dictionary, and a
            //   `text` column is not single-valued at all.
            if !attribute.index {
                return Err(declaration_error(format!(
                    "layer '{}': `membership = {{ attribute = \"{field}\" }}` names a column that \
                     is not indexed. The predicate reads the entity-addressed value column that \
                     `index = true` writes, and without one there is nothing for it to evaluate",
                    block.name
                )));
            }
            if !attribute.ty.is_category_width() {
                return Err(declaration_error(format!(
                    "layer '{}': `membership = {{ attribute = \"{field}\" }}` names a `{}` \
                     column. ⊘ An attribute membership is a predicate over a **single-valued \
                     category-width** column — `u8`, `u16` or `u32`, with or without a vocabulary \
                     — because such a column partitions the corpus: every point carries exactly \
                     one value, so the values are the artifacts and one label per row is the whole \
                     membership. A wider or non-integer column has no code to label a row with",
                    block.name,
                    attribute.ty.arrow_type_name()
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

/// `[layer.shape]` — what kind of shape a spatial layer's artifacts carry
/// (`polygon-membership.md` §6.1): `bbox`, `circle`, `ellipse` or `polygon`, and nothing else.
///
/// **No depth.** Every kind is exact — the members are the rows whose stored position is inside
/// the shape — so there is nothing for a depth to hold, and one written is refused naming where it
/// went rather than as an unknown key: a declaration carrying the cover-at-depth form of an
/// earlier surface should be told what replaced it.
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
             predicate the membership names, and the shape beside them would decide nothing",
            block.name
        )));
    }
    if declared.depth.is_some() {
        return Err(declaration_error(format!(
            "layer '{}': `shape.depth` is not a key. Every shape kind is exact — the members are \
             the rows inside the shape, tested one by one at the boundary — so there is no depth \
             to declare and no cover to be drawn at one (`polygon-membership.md` §6.1). Remove \
             the key",
            block.name
        )));
    }
    let kind = match declared.kind.as_deref() {
        Some(word) => ShapeKind::parse(word).ok_or_else(|| {
            declaration_error(format!(
                "layer '{}': `shape.kind = \"{word}\"` is not a shape kind; the kinds are {}. Each \
                 artifact then carries its geometry in that kind's fields — `bbox`, `circle`, \
                 `ellipse`, or `wkt` inline and a WKB `geometry` column in a table",
                block.name,
                ShapeKind::VOCABULARY.join(", ")
            ))
        })?,
        None => {
            return Err(declaration_error(format!(
                "layer '{}': `[layer.shape]` declares no `kind`; the kinds are {}",
                block.name,
                ShapeKind::VOCABULARY.join(", ")
            )))
        }
    };
    Ok(Some(ShapeDeclaration { kind }))
}

fn compile_hierarchy(block: &LayerBlock) -> Result<Hierarchy> {
    let declared = block.hierarchy.as_ref().ok_or_else(|| {
        declaration_error(format!(
            "layer '{}': `hierarchy` is required — the kind is declared and never inferred from \
             the edges. Write `hierarchy = {{ kind = \"flat\" }}` for one population of artifacts, \
             \"nested\" for a tree held in the edges, \"dag\" for the same with a child under \
             several parents, \"stacked\" for independent analyses one per level, or \"tiered\" \
             for containment edges running coarser → finer between levels. `prune_children = \
             true` serves only the deepest passing artifact per branch",
            block.name
        ))
    })?;
    let kind = match declared.kind.as_deref() {
        Some("flat") => HierarchyKind::Flat,
        Some("nested") => HierarchyKind::Nested,
        Some("dag") => HierarchyKind::Dag,
        Some("stacked") => HierarchyKind::Stacked,
        Some("tiered") => HierarchyKind::Tiered,
        Some(other) => {
            return Err(declaration_error(format!(
                "layer '{}': `hierarchy.kind = \"{other}\"` is none of \"flat\", \"nested\", \
                 \"dag\", \"stacked\" or \"tiered\"",
                block.name
            )));
        }
        None => {
            return Err(declaration_error(format!(
                "layer '{}': `hierarchy.kind` is required. \"flat\" is one population with no \
                 lineage; \"nested\" is a tree held in the edges, every artifact at level 0; \
                 \"dag\" is the same with a child under several parents; \"stacked\" is \
                 independent analyses, one per level; \"tiered\" is containment edges running \
                 coarser → finer between levels",
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

// ---------------------------------------------------------------------------------------------
// The build's view registry (`views.md` §7)
// ---------------------------------------------------------------------------------------------

/// A view of a group, as the registry records it (`views.md` §3.2).
#[derive(Debug, Clone)]
pub struct GroupMembership {
    /// The group this view belongs to. `<group>:<key>` is the view id.
    pub group: String,
    pub key: String,
    /// The group whose keys these are, where this group declares `members`
    /// (`views.md` §3.3); `None` where it owns them. Metadata and each view's own gate belong to
    /// the owner, so a `members` group's views carry none of their own.
    pub members_of: Option<String>,
    /// This view's typed metadata, one entry per name the owning group declared.
    pub metadata: BTreeMap<String, MetadataValue>,
}

/// How one view's rows are picked out of a shared points file (`views.md` §3.1's form B).
///
/// **The roster is what makes the selection checkable.** A row carrying a key the group's roster
/// does not list belongs to no view, so it is refused naming the key and the roster rather than
/// dropped: dropping it would build a bundle quietly missing the rows of a view nobody declared,
/// which is the failure form B's refusal exists to prevent. A listed key with no rows is the
/// other case entirely, and is an empty view.
#[derive(Debug, Clone)]
pub struct ViewSelector {
    /// The discriminator column — `fields.view` on the group, resolved.
    pub column: String,
    /// The value this view's rows carry: the view's key.
    pub value: String,
    /// Every key the group's roster carries, sorted, so a row's key is a binary search and an
    /// unknown one is a refusal that can list the alternatives.
    pub keys: Vec<String>,
    /// The view whose rows these are, for the refusal to name.
    pub view_id: String,
}

/// One coordinate system a build materialises (`views.md` §7).
///
/// **The registry is ordered, and the order is a contract**: it decides which view's Morton code
/// an item absent from the anchor is tie-broken on (decision 0112), and a group's views keep
/// their roster order within it. The order is the plain `[[view]]` blocks in declaration
/// order, then each `[[view_group]]` in declaration order with its views in roster order —
/// `Config` holds the two block kinds in separate lists, so their interleaving in the document is
/// not recoverable and is deliberately not part of the order.
#[derive(Debug, Clone)]
pub struct BuildView {
    /// The view id: a plain view's `name`, or a group's view as the joined `group:key` form.
    pub id: String,
    /// `None` for a plain view.
    pub group: Option<GroupMembership>,
    pub projection: Projection,
    pub extent: Extent,
    /// This view's points. `None` is a view declared and empty, which is legal to declare and
    /// refused at a build that would have to read it.
    pub source: Option<PathBuf>,
    pub fields: Fields,
    pub point_visibility: PointVisibility,
    /// Form B: how this view's rows are picked out of a shared points file (`views.md` §3.1).
    /// `None` where the file *is* the view — every plain view, and every view of a form A group.
    pub select: Option<ViewSelector>,
    /// This view's own gate, a list of labels each one term (`views.md` §6, decision 0132);
    /// `None` takes its group's.
    pub visibility: Option<Vec<String>>,
}

/// The roster table's rows as roster records (`views.md` §3.1's form B).
///
/// **The same rules the inline block is held to**, applied to a file: the key charset, no key
/// twice, and each view's own gate through the one gate compiler — so which form a corpus wrote
/// its roster in changes nothing about what the roster may say.
fn read_roster(group: &ViewGroup, table: &RosterTable) -> Result<Vec<RosterView>> {
    let object = format!("view group '{}' `[view_group.views]`", group.name);
    let rows = crate::input::read_roster_table(&table.source, &table.fields, &group.metadata)?;
    if rows.is_empty() {
        return Err(declaration_error(format!(
            "{object}: {} carries no rows, so this group has no views. A group with none is a \
             declaration promising coordinate systems the bundle would not carry (views §3.1)",
            table.source.display()
        )));
    }
    let mut roster: Vec<RosterView> = Vec::with_capacity(rows.len());
    for row in rows {
        let view = format!("{object}, view '{}'", row.key);
        check_view_name(&view, &row.key)?;
        if roster.iter().any(|v| v.key == row.key) {
            return Err(declaration_error(format!(
                "{object}: view key '{}' appears twice. A key is the caller's own name for one \
                 view, tombstoned on drop and never reused, and `<group>:<key>` is the id every \
                 request and every stored path is written under (views §3.2)",
                row.key
            )));
        }
        roster.push(RosterView {
            key: row.key,
            // Form B's points are the group's own file, selected by the discriminator: a roster
            // row names no source of its own.
            source: None,
            visibility: compile_view_gate(&view, row.visibility.as_deref())?,
            metadata: row.metadata,
        });
    }
    Ok(roster)
}

/// Mint a roster from the distinct values of a group's discriminator (`views.md` §3.1's third
/// form): one view per distinct value, carrying no metadata and no gate of its own.
///
/// **The mint produces ordinary roster records**, which is the whole of the design: everything
/// below this point — the registry, the manifest's `GroupDescriptor`, the create and drop verbs,
/// the join rule — sees a roster it cannot tell from a written one. A minted group is a group
/// whose keys were read from the data rather than typed.
///
/// Three rules the declaration does not decide, chosen here as recoverable defaults (architect's
/// choices, 2026-08-31; `views.md` §3.1 records them):
///
/// - **Key-byte sorted order**, so the served order — roster order, since decision 0113 — is a
///   property of the key set rather than of how the source's rows happen to be arranged.
/// - **A value that cannot be a key is a refusal**, not a skip and not a mangling: a skipped
///   value's rows would belong to no view, which is the refusal a stray key already earns.
/// - **Every minted view takes the group's own gate**, there being no roster record on which a
///   narrower one could be written.
///
/// **`scanned` is the pass's memo of what each file has already said**, keyed by the file and the
/// column read from it. The mint is a full pass over a *points* file — the largest input a group
/// has — and it is asked for once per group whose roster is this one: an owner and every `members`
/// group naming it resolve to the same owner and would each pay for the same scan.
fn mint_roster(
    group: &ViewGroup,
    scanned: &mut HashMap<(PathBuf, String), BTreeSet<String>>,
) -> Result<Vec<RosterView>> {
    let object = format!("view group '{}'", group.name);
    let source = group
        .source
        .as_ref()
        .expect("a group declaring no roster declares a source, or the declaration was refused");
    let column = group.fields.of("view");
    let keys = match scanned.entry((source.clone(), column.to_string())) {
        std::collections::hash_map::Entry::Occupied(held) => held.into_mut(),
        std::collections::hash_map::Entry::Vacant(empty) => {
            empty.insert(crate::input::read_discriminator_keys(source, column)?)
        }
    };
    if keys.is_empty() {
        return Err(declaration_error(format!(
            "{object}: {} carries no rows, so this group has no views. Its views are the distinct \
             values of the discriminator column '{column}', and a group with no views is a \
             declaration promising coordinate systems the bundle would not carry (views §3.1)",
            source.display()
        )));
    }
    keys.iter()
        .map(|key| {
            tessera_types::view::check_view_key(key).map_err(|detail| {
                declaration_error(format!(
                    "{object}: the discriminator column '{column}' carries the value '{key}', and \
                     this group declares no roster, so every distinct value of that column is a \
                     view key — {detail}. Refused rather than skipped or rewritten: a value this \
                     build did not mint a view for is one whose rows belong to no view (views \
                     §3.1, §3.2). Write `[view_group.views]` to name the views instead"
                ))
            })?;
            Ok(RosterView {
                key: key.clone(),
                // As form B: the points are the group's own file, selected by the discriminator.
                source: None,
                // No roster record, so no gate of its own — the group's is the one it takes,
                // applied by the registry below.
                visibility: None,
                metadata: BTreeMap::new(),
            })
        })
        .collect()
}

impl Config {
    /// Every coordinate system a build materialises, in the registry's order
    /// ([`BuildView`], `views.md` §7).
    ///
    /// **A build materialises every declared view and every view of every group.** What it cannot
    /// yet enumerate is refused rather than silently dropped: a bundle whose declaration promises
    /// coordinate systems it does not carry is the failure this refusal exists to prevent.
    pub fn build_views(&self) -> Result<Vec<BuildView>> {
        // What each points file has already said about its discriminator, so a group's source is
        // scanned once however many groups mint their roster from it ([`mint_roster`]).
        let mut scanned: HashMap<(PathBuf, String), BTreeSet<String>> = HashMap::new();
        let mut registry: Vec<BuildView> = self
            .views
            .iter()
            .map(|view| BuildView {
                id: view.name.clone(),
                group: None,
                projection: view.projection,
                extent: view.extent,
                source: view.source.clone(),
                fields: view.fields.clone(),
                point_visibility: view.point_visibility.clone(),
                select: None,
                visibility: view.visibility.clone(),
            })
            .collect();
        for group in &self.view_groups {
            let owner = match &group.members {
                None => group,
                Some(name) => self
                    .view_groups
                    .iter()
                    .find(|g| &g.name == name)
                    .ok_or_else(|| {
                        declaration_error(format!(
                            "view group '{}': `members = \"{name}\"` names no `[[view_group]]` \
                             block",
                            group.name
                        ))
                    })?,
            };
            // **The roster's rows are views**, whichever form declared them: inline blocks, or
            // the rows of `[view_group.views].source` read here — before pass two, because the
            // registry is what pass two iterates and a key the table carries is a coordinate
            // system this build materialises (`views.md` §3.1, §7).
            let roster: Vec<RosterView> = match &owner.roster {
                Roster::Inline(views) => views.clone(),
                Roster::Table(table) => read_roster(owner, table)?,
                // A group declaring no roster has its keys minted from the distinct values of
                // its own discriminator, read here for the reason the table is: the registry is
                // what pass two iterates, and a minted key is a coordinate system this build
                // materialises (`views.md` §3.1, §7).
                Roster::Discriminator => mint_roster(owner, &mut scanned)?,
            };
            let discriminator_field = group.fields.of("view").to_string();
            // Sorted once per group, not once per view: it is the same roster each of its views
            // checks a stray key against.
            let mut keys: Vec<String> = roster.iter().map(|v| v.key.clone()).collect();
            keys.sort();
            for view in roster.iter() {
                // Form A gives each view its own file, so there is nothing to select on; a group
                // carrying its own `source` — form B, and every `members` group — holds every
                // view's points in one file and selects by key.
                let id = format!("{}:{}", group.name, view.key);
                let (source, select) = match (&group.source, &view.source) {
                    (Some(shared), _) => (
                        Some(shared.clone()),
                        Some(ViewSelector {
                            column: discriminator_field.clone(),
                            value: view.key.clone(),
                            keys: keys.clone(),
                            view_id: id.clone(),
                        }),
                    ),
                    (None, own) => (own.clone(), None),
                };
                registry.push(BuildView {
                    id,
                    group: Some(GroupMembership {
                        group: group.name.clone(),
                        key: view.key.clone(),
                        members_of: group.members.clone(),
                        metadata: if group.members.is_some() {
                            BTreeMap::new()
                        } else {
                            view.metadata.clone()
                        },
                    }),
                    projection: group.projection,
                    extent: group.extent,
                    source,
                    fields: group.fields.clone(),
                    point_visibility: group.point_visibility.clone(),
                    select,
                    visibility: if group.members.is_some() {
                        group.visibility.clone()
                    } else {
                        view.visibility.clone().or_else(|| group.visibility.clone())
                    },
                });
            }
        }
        if registry.is_empty() {
            return Err(declaration_error(
                "the declaration has no `[[view]]` block and no `[[view_group]]`, so this build \
                 has no coordinate system to materialise. A view names the geometry source and \
                 the frame it is quantised against (configuration.md §1)",
            ));
        }
        Ok(registry)
    }

    /// The views a layer is drawn on, expanded against a [`Config::build_views`] registry: a
    /// plain view under its own name, and **a group under every view of it** (`views.md` §2,
    /// §3.5).
    ///
    /// Naming a group draws the layer on every view of it, which is what lets a layer follow a
    /// group that grows rather than being redeclared per quarter. A name matching no group is a
    /// plain view and is kept as written — the declaration refused an unknown one long before
    /// this.
    ///
    /// ⊘ *Present and future* is the ingest half: a view created later gets the layer's artifacts
    /// at the fold that writes them (spec §3.5).
    pub fn expand_layer_views(registry: &[BuildView], declared: &[String]) -> Vec<String> {
        let mut expanded = Vec::with_capacity(declared.len());
        for name in declared {
            let of_group: Vec<String> = registry
                .iter()
                .filter(|view| view.group.as_ref().is_some_and(|g| &g.group == name))
                .map(|view| view.id.clone())
                .collect();
            match of_group.is_empty() {
                true => expanded.push(name.clone()),
                false => expanded.extend(of_group),
            }
        }
        expanded
    }

    /// The group registry the manifest publishes, derived from a [`Config::build_views`]
    /// registry (`views.md` §3.2).
    ///
    /// **The roster's durable home is the manifest** — one place, carried forward for ever, for
    /// the reason `entity_id_low_water` and `layer_tombstones` are there (decision 0029).
    pub fn group_registry(
        &self,
        registry: &[BuildView],
        resolved: &[crate::ViewArgs],
    ) -> Vec<tessera_store::manifest::GroupDescriptor> {
        use tessera_store::manifest::{
            GroupDescriptor, GroupMetadataField, GroupViewDescriptor, ViewMetadataValue,
        };
        let mut groups: Vec<GroupDescriptor> = Vec::new();
        for view in registry {
            let Some(membership) = &view.group else {
                continue;
            };
            if !groups.iter().any(|g| g.name == membership.group) {
                // **The group's own settings, published beside the roster** — the frame, the
                // projection and the declared metadata names. A view created while the service
                // runs is minted from them (`views.md` §3.2), and a group whose roster is empty
                // has no view to read them off.
                let declared = self
                    .view_groups
                    .iter()
                    .find(|g| g.name == membership.group)
                    .expect("every group in the registry was compiled from a declaration");
                let owner = match &declared.members {
                    None => declared,
                    Some(name) => self
                        .view_groups
                        .iter()
                        .find(|g| &g.name == name)
                        .expect("a members chain is refused at the declaration"),
                };
                let frame = resolved
                    .iter()
                    .find(|v| v.view_id == view.id)
                    .map(|v| v.extent)
                    .expect("every view in the registry is materialised by this build");
                groups.push(GroupDescriptor {
                    name: membership.group.clone(),
                    // The **declared** group's title, not the owner's, for the reason its gate is
                    // its own: two groups over one key set are two layouts, and how a layout is
                    // presented is a fact about the layout (`views.md` §3.3).
                    title: declared.title.clone(),
                    members_of: membership.members_of.clone(),
                    // The declared group's own point default (decision 0133), on the rule its
                    // gate and title follow: two groups over one key set are two declarations.
                    point_default: declared.point_visibility.default.clone(),
                    // **The frame as resolved, not as declared**: a group's `auto` extent is
                    // fitted over every view of it, so the declaration may say `auto` where the
                    // manifest must say numbers. Every view of a group shares one frame by
                    // construction (`views.md` §3.1), so any of them answers — and this view is
                    // one of them.
                    quantisation: tessera_store::manifest::Quantisation {
                        x_min: frame.x_min,
                        x_max: frame.x_max,
                        y_min: frame.y_min,
                        y_max: frame.y_max,
                    },
                    projection: declared.projection,
                    // **The group's own gate, the outer bound over every view of it**
                    // (`views.md` §6). The *declared* group's, not the owner's: two groups sharing
                    // one key set are two layouts, and which principals may see each layout is a
                    // fact about the layout (`views.md` §3.3).
                    visibility: declared.visibility.clone(),
                    // A `members` group declares none: keys and metadata belong to the
                    // group that owns the views (`views.md` §3.3), so a create against the owner
                    // is what supplies them and this group's copies carry none.
                    metadata: if declared.members.is_some() {
                        Vec::new()
                    } else {
                        owner
                            .metadata
                            .iter()
                            // The mapping is [`ViewMetadata::declared_type`], shared with the
                            // control-plane emitter so a build and a runtime declaration of one
                            // block cannot disagree about a name's type (decision 0139).
                            .map(|field| GroupMetadataField {
                                name: field.name.clone(),
                                ty: field.declared_type(),
                                vocabulary: field.vocabulary.clone(),
                            })
                            .collect()
                    },
                    views: Vec::new(),
                    // Filled at the manifest write from the declaration's scoped attributes
                    // (`tessera_build::build`), so the family list has one origin.
                    scoped_scalars: Vec::new(),
                });
            }
            let group = groups
                .iter_mut()
                .find(|g| g.name == membership.group)
                .expect("just inserted");
            group.views.push(GroupViewDescriptor {
                key: membership.key.clone(),
                visibility: view.visibility.clone(),
                metadata: membership
                    .metadata
                    .iter()
                    .map(|(name, value)| {
                        let value = match value {
                            MetadataValue::Bool(v) => ViewMetadataValue::Bool(*v),
                            MetadataValue::Int(v) => ViewMetadataValue::Int(*v),
                            MetadataValue::Float(v) => ViewMetadataValue::Float(*v),
                            MetadataValue::Text(v) => ViewMetadataValue::Text(v.clone()),
                            MetadataValue::TimestampUs(v) => ViewMetadataValue::TimestampUs(*v),
                        };
                        (name.clone(), value)
                    })
                    .collect(),
            });
        }
        groups
    }

    /// Which of `registry` is the **anchor view** — the one whose Morton code breaks entity-id
    /// ties within a signature group ([decision 0112](../decisions/0112-the-anchor-view-orders-a-signature-groups-ids.md)).
    ///
    /// **Required when more than one view is declared, and refused absent naming the candidates.**
    /// Entity ids are permanent (I9), so the tie-break is a permanent property of the corpus: a
    /// positional default would let reordering two declaration blocks silently re-key a rebuild.
    /// With one view it is that view, and naming it is noise.
    pub fn anchor_view(&self, registry: &[BuildView]) -> Result<usize> {
        let candidates = || names(registry.iter().map(|v| v.id.as_str()));
        match (&self.allocation_view, registry.len()) {
            (Some(named), _) => registry.iter().position(|v| &v.id == named).ok_or_else(|| {
                declaration_error(format!(
                    "[defaults].allocation_view = \"{named}\" names no view this build \
                         materialises. Declared: {}. The anchor is one coordinate system, so a \
                         group is named through one of its views — `<group>:<key>` (views §3.2, \
                         decision 0112)",
                    candidates()
                ))
            }),
            (None, 1) => Ok(0),
            (None, _) => Err(declaration_error(format!(
                "the declaration carries {} views and `[defaults].allocation_view` names none. \
                 Entity ids are assigned once and are permanent (I9), and within a signature \
                 group they are ordered by the item's Morton code in the anchor view — so with \
                 several views the anchor is a declaration rather than a default, or reordering \
                 two blocks would silently re-key a rebuild (decision 0112). Name one of: {}",
                registry.len(),
                candidates()
            ))),
        }
    }
}

// ---------------------------------------------------------------------------------------------
// The control-plane payloads
// ---------------------------------------------------------------------------------------------

/// The declaration, minus its acquisition keys, as the control plane's declaration routes take it
/// (`configuration.md` §2; `ingest.md` §1.3): one array per runtime block kind, each in
/// declaration order.
///
/// **One implementation of declaration-to-payload** ([decision 0139](../../../docs/decisions/0139-one-implementation-between-build-and-ingest-and-across-a-type-family.md)).
/// This serialises the same parsed types the build compiles from, so a key the parser accepts and
/// this does not is a key the emitter can be seen to drop, rather than one a second reading of the
/// TOML never knew about.
///
/// **The emitter states the declaration; the route decides.** Where a compiled block carries
/// something a running service refuses — `render` on an attribute (decision 0136's amendment), an
/// `auto` extent, which has no data to fit against here — it is emitted as declared and the route
/// answers. An emitter that pre-filtered would hide the refusal instead of delivering it.
///
/// **Where the name travels decides the shape.** A layer body and an attribute body carry their
/// own `name`, so those two arrays are bare bodies; a vocabulary, a view and a view group are
/// addressed by a path segment, so each of those entries is `{ "name", "body" }` and the body is
/// what goes on the wire. A vocabulary's entry carries a third key: `values`, the
/// `PATCH /control/vocabularies/{name}/values` page its inline values make — or `values_source`,
/// the `[sources]` name a sourced value set reads, whose values are rows rather than declaration
/// and are not emitted. **A sourced closed set is therefore a declaration the route refuses**: a
/// closed set with no values refuses every ingest, so its keys have to be paged before it can be
/// declared, and `values_source` says where they are.
pub fn control_payloads(config: &Config) -> serde_json::Value {
    serde_json::json!({
        "layers": config.layers,
        "attributes": attribute_payloads(config),
        "vocabularies": vocabulary_payloads(config),
        "views": view_payloads(config),
        "view_groups": view_group_payloads(config),
    })
}

/// Drop the key where the value is absent: every optional on a declaration route is
/// `#[serde(default)]`, and `deny_unknown_fields` is the reason nothing is invented to fill one.
fn insert_some<T: serde::Serialize>(
    body: &mut serde_json::Map<String, serde_json::Value>,
    key: &str,
    value: Option<T>,
) {
    if let Some(value) = value {
        body.insert(
            key.to_string(),
            serde_json::to_value(value).unwrap_or_default(),
        );
    }
}

/// One `PUT /control/attributes` body per declared column, entity-scoped and group-scoped alike,
/// in declaration order.
///
/// **`field`, `source`, `entity_id_field` and `fields` are the acquisition half** — where a build
/// reads the column from — and are gone by this point: an [`Attribute`] carries `field` because a
/// build needs it, and nothing else here does.
fn attribute_payloads(config: &Config) -> Vec<serde_json::Value> {
    let mut out = Vec::with_capacity(config.attribute_order.len());
    for name in &config.attribute_order {
        let (attribute, scope) = match config.schema.attributes.iter().find(|a| &a.name == name) {
            Some(attribute) => (attribute, tessera_types::layer::LayerScope::Entity),
            None => match config
                .scoped_attributes
                .iter()
                .find(|a| &a.attribute.name == name)
            {
                Some(scoped) => (
                    &scoped.attribute,
                    tessera_types::layer::LayerScope::Group(scoped.group.clone()),
                ),
                // A name in the order list that compiled to no column is not reachable: both
                // halves are pushed from the same block list. Skipped rather than asserted,
                // because an emitter is not the place to panic over a declaration.
                None => continue,
            },
        };
        out.push(attribute_payload(attribute, scope));
    }
    out
}

fn attribute_payload(
    attribute: &Attribute,
    scope: tessera_types::layer::LayerScope,
) -> serde_json::Value {
    let mut body = serde_json::Map::new();
    body.insert("name".to_string(), attribute.name.clone().into());
    insert_some(&mut body, "title", attribute.title.clone());
    // **A category is spelled as the block spells it**: `type = "category"` with the vocabulary
    // beside it and the code space's width in `width`. The compiled type is that width
    // (`Attribute::ty`), so writing it into `type` would state the storage where the declaration
    // stated the kind.
    match &attribute.vocabulary {
        Some(vocabulary) => {
            body.insert("type".to_string(), "category".into());
            body.insert("vocabulary".to_string(), vocabulary.clone().into());
            body.insert("width".to_string(), attribute.ty.arrow_type_name().into());
        }
        None => {
            body.insert("type".to_string(), attribute.ty.arrow_type_name().into());
        }
    }
    // **The declared name, not the resolved identity.** A `text` column's analyser is resolved to
    // `<name>/<version>` at parse (decision 0070) and the route resolves it again from the name,
    // so the version is this binary's answer rather than anything the author wrote.
    insert_some(
        &mut body,
        "analyser",
        attribute
            .analyser
            .as_deref()
            .and_then(|identity| identity.split('/').next())
            .map(str::to_string),
    );
    body.insert("index".to_string(), attribute.index.into());
    body.insert("render".to_string(), attribute.render.into());
    body.insert(
        "scope".to_string(),
        serde_json::to_value(scope).unwrap_or_default(),
    );
    serde_json::Value::Object(body)
}

/// One vocabulary per declared block, in declaration order: the `PUT /control/vocabularies/{name}`
/// body, and beside it either the values page or the source its values are rows from.
fn vocabulary_payloads(config: &Config) -> Vec<serde_json::Value> {
    let mut out = Vec::with_capacity(config.vocabulary_order.len());
    for name in &config.vocabulary_order {
        let Some(vocabulary) = config.schema.vocabularies.get(name) else {
            continue;
        };
        let mut body = serde_json::Map::new();
        insert_some(&mut body, "title", vocabulary.title.clone());
        body.insert(
            "value_set".to_string(),
            match vocabulary.value_set {
                ValueSet::Closed => "closed",
                ValueSet::Open => "open",
            }
            .into(),
        );
        body.insert(
            "visibility".to_string(),
            serde_json::to_value(vocabulary.visibility).unwrap_or_default(),
        );
        body.insert(
            "width".to_string(),
            vocabulary.width.arrow_type_name().into(),
        );
        if !vocabulary.reserved.is_empty() {
            body.insert(
                "reserved".to_string(),
                serde_json::to_value(&vocabulary.reserved).unwrap_or_default(),
            );
        }

        let mut entry = serde_json::Map::new();
        entry.insert("name".to_string(), name.clone().into());
        match config.vocabulary_sources.get(name) {
            // **A sourced value set is rows, not declaration.** The file is read at a build and
            // its keys are data; emitting them here would put a corpus's contents in a payload a
            // declare-only deployment posts before it has any.
            Some(source) => {
                entry.insert("values_source".to_string(), source.clone().into());
            }
            None if !vocabulary.codes.is_empty() => {
                // **No `code` anywhere.** Codes are the server's to assign
                // (`per-point-attributes.md` §3.1), and the route refuses a body that names one —
                // so an inline `key = code` table reaches the wire as its keys and titles, and the
                // running service draws the codes.
                let values: Vec<serde_json::Value> = vocabulary
                    .codes
                    .keys()
                    .map(|key| {
                        let mut row = serde_json::Map::new();
                        row.insert("key".to_string(), key.clone().into());
                        insert_some(&mut row, "title", vocabulary.titles.get(key).cloned());
                        serde_json::Value::Object(row)
                    })
                    .collect();
                // **On the declaration body and beside it.** A closed set with no values is
                // refused at the route — an authored set of nothing refuses every ingest — so the
                // values travel with the declaration that needs them; the page is the same rows
                // as `PATCH /control/vocabularies/{name}/values` takes, which is how a value set
                // grows after its declaration (`ingest.md` §1.3).
                body.insert("values".to_string(), values.clone().into());
                entry.insert(
                    "values".to_string(),
                    serde_json::json!({ "values": values }),
                );
            }
            None => {}
        }
        entry.insert("body".to_string(), serde_json::Value::Object(body));
        out.push(serde_json::Value::Object(entry));
    }
    out
}

/// One `PUT /control/views/{name}` body per plain `[[view]]` block, in declaration order.
///
/// **A group's views are not here.** They are created through the roster route
/// (`PUT /control/views/{group}/{key}`), whose body is a roster record rather than a declaration.
fn view_payloads(config: &Config) -> Vec<serde_json::Value> {
    config
        .views
        .iter()
        .map(|view| {
            let mut body = serde_json::Map::new();
            insert_some(&mut body, "title", view.title.clone());
            body.insert("projection".to_string(), view.projection.name().into());
            body.insert(
                "extent".to_string(),
                extent_payload(view.projection, &view.extent),
            );
            insert_some(&mut body, "visibility", view.visibility.clone());
            insert_some(
                &mut body,
                "point_visibility",
                point_visibility_payload(&view.point_visibility),
            );
            serde_json::json!({ "name": view.name, "body": serde_json::Value::Object(body) })
        })
        .collect()
}

/// One `PUT /control/view_groups/{name}` body per `[[view_group]]` block, in declaration order.
///
/// The roster and the group's own points file are acquisition and are gone: a group declared at a
/// running service starts with an empty roster, and its keys are created one at a time.
fn view_group_payloads(config: &Config) -> Vec<serde_json::Value> {
    config
        .view_groups
        .iter()
        .map(|group| {
            let mut body = serde_json::Map::new();
            insert_some(&mut body, "title", group.title.clone());
            body.insert("projection".to_string(), group.projection.name().into());
            body.insert(
                "extent".to_string(),
                extent_payload(group.projection, &group.extent),
            );
            insert_some(&mut body, "visibility", group.visibility.clone());
            insert_some(
                &mut body,
                "point_visibility",
                point_visibility_payload(&group.point_visibility),
            );
            insert_some(&mut body, "members", group.members.clone());
            let metadata: Vec<serde_json::Value> = group
                .metadata
                .iter()
                .map(|field| {
                    let mut entry = serde_json::Map::new();
                    entry.insert("name".to_string(), field.name.clone().into());
                    entry.insert(
                        "type".to_string(),
                        serde_json::to_value(field.declared_type()).unwrap_or_default(),
                    );
                    insert_some(&mut entry, "vocabulary", field.vocabulary.clone());
                    serde_json::Value::Object(entry)
                })
                .collect();
            if !metadata.is_empty() {
                body.insert("metadata".to_string(), metadata.into());
            }
            serde_json::json!({ "name": group.name, "body": serde_json::Value::Object(body) })
        })
        .collect()
}

/// `point_visibility` as a declaration at a running service spells it: the default alone.
///
/// `field` and `source` are where a build reads each point's own label; a batch carries its own
/// `access` list, so the only half that crosses is what a point carrying none is given
/// (decision 0133). A block that declared only those two leaves nothing to send, and the key is
/// dropped rather than sent empty.
fn point_visibility_payload(visibility: &PointVisibility) -> Option<serde_json::Value> {
    visibility
        .default
        .as_ref()
        .map(|default| serde_json::json!({ "default": default }))
}

/// The frame, in the coordinates the route takes: the four bounds a Morton code is a fraction of.
///
/// **A projected view's declared degree box is snapped here, by the routine the build snaps with**
/// (`projections.md` §4.2), so a view declared at a running service lands on the frame a build of
/// the same declaration would have given it — not on the unsnapped box.
///
/// **`auto` has nothing to resolve against** and is emitted as declared. There is no source to
/// survey at a running service, so the route refuses it naming the key; an emitter that guessed a
/// box would give a view a frame nobody chose, and a frame is immutable for the view's life
/// (decision 0040).
fn extent_payload(projection: Projection, extent: &Extent) -> serde_json::Value {
    match extent {
        Extent::Fixed(bounds) => serde_json::json!({
            "x": [bounds.x_min, bounds.x_max],
            "y": [bounds.y_min, bounds.y_max],
        }),
        Extent::LonLat(box_) => {
            let bounds = snap_lon_lat(projection, box_).square.bounds();
            serde_json::json!({
                "x": [bounds.x_min, bounds.x_max],
                "y": [bounds.y_min, bounds.y_max],
            })
        }
        Extent::Auto { margin } => serde_json::json!({ "auto": true, "margin": margin }),
        Extent::AutoLonLat => serde_json::json!({ "auto": true }),
    }
}

// ---------------------------------------------------------------------------------------------
// The control-plane payloads' own tests
// ---------------------------------------------------------------------------------------------

/// [`control_payloads`] over a declaration that writes every key each block takes.
///
/// **One test a block kind**, each asking the same two questions: is every acquisition key gone,
/// and is everything else there in the shape the route takes? The round trip — that a running
/// service accepts what comes out — is `tessera-server`'s `tests/payload_emitter.rs`, because only
/// a server can answer it.
#[cfg(test)]
mod payload_tests {
    use super::*;

    /// A vocabulary's values read from a file: the one block whose parse opens anything, and the
    /// case `values_source` exists for.
    fn write_value_file(path: &Path) {
        use arrow::array::{ArrayRef, StringArray};
        use arrow::datatypes::{DataType, Field};
        use std::sync::Arc;
        let schema = Arc::new(arrow::datatypes::Schema::new(vec![
            Field::new("name", DataType::Utf8, false),
            Field::new("title", DataType::Utf8, true),
        ]));
        let keys: ArrayRef = Arc::new(StringArray::from(vec!["north", "south"]));
        let titles: ArrayRef = Arc::new(StringArray::from(vec!["North", "South"]));
        let batch =
            arrow::record_batch::RecordBatch::try_new(schema.clone(), vec![keys, titles]).unwrap();
        let mut writer = parquet::arrow::ArrowWriter::try_new(
            std::fs::File::create(path).unwrap(),
            schema,
            None,
        )
        .unwrap();
        writer.write(&batch).unwrap();
        writer.close().unwrap();
    }

    /// Every block kind, every key: two plain views (one projected, one not), a form B view group
    /// with a roster table and metadata of three types, two vocabularies (inline with pinned codes
    /// and reserved, and sourced), four attributes (plain, category, text, group-scoped) and two
    /// layers.
    const EVERY_KEY: &str = r#"
[sources]
points   = "points.parquet"
extra    = "extra.parquet"
regions  = "regions.parquet"
quarters = "quarters.parquet"
roster   = "roster.parquet"
members  = "members.parquet"
clusters = "clusters.parquet"

[defaults]
source          = "points"
entity_id_field = "id"
allocation_view = "world"

[[view]]
name             = "world"
title            = "Whole corpus"
projection       = "web_mercator"
source           = "points"
extent           = { lon = [-180.0, 180.0], lat = [-85.0511287798066, 85.0511287798066] }
visibility       = "ir:analyst"
point_visibility = { field = "access", default = "public" }

[[view]]
name             = "embedding"
extent           = { x = [0.0, 1000.0], y = [0.0, 1000.0] }
point_visibility = { default = "public" }

[[view_group]]
name             = "quarter"
title            = "By quarter"
projection       = "none"
source           = "quarters"
fields           = { view = "quarter" }
extent           = { x = [-40.0, 40.0], y = [-40.0, 40.0] }
visibility       = "ir:analyst"
point_visibility = { field = "access", default = "public" }
metadata         = { label = "text", starts = "timestamp_us", region = { type = "category", vocabulary = "region" } }

[view_group.views]
source = "roster"
fields = { key = "quarter" }

[[vocabulary]]
name       = "kind"
title      = "Feature kind"
width      = "u8"
value_set  = "closed"
visibility = "public"
values     = { alpha = 3, beta = 7 }
reserved   = [9]

[[vocabulary]]
name       = "region"
title      = "Region"
width      = "u16"
value_set  = "closed"
visibility = "derived"
source     = "regions"
fields     = { key = "name" }

[[attribute]]
name            = "importance"
title           = "Importance"
field           = "pop"
source          = "extra"
entity_id_field = "row_id"
type            = "u32"
index           = true
render          = true

[[attribute]]
name       = "feature"
title      = "Feature class"
type       = "category"
vocabulary = "kind"
render     = true

[[attribute]]
name     = "note"
type     = "text"
analyser = "unicode"
index    = true

[[attribute]]
name            = "coverage"
type            = "f32"
source          = "quarters"
entity_id_field = "id"
fields          = { view = "quarter" }
scope           = { group = "quarter" }

[[layer]]
name       = "clusters/a"
title      = "Clusters"
views      = ["world"]
source     = "clusters"
membership = "enumerated"
hierarchy  = { kind = "flat" }
visibility = "ir:analyst"
artifact_visibility       = { default = "inherited" }
require_member_visibility = "any"

[layer.members]
source = "members"
"#;

    fn every_key() -> (tempfile::TempDir, serde_json::Value) {
        let dir = tempfile::tempdir().unwrap();
        write_value_file(&dir.path().join("regions.parquet"));
        let path = dir.path().join("config.toml");
        std::fs::write(&path, EVERY_KEY).unwrap();
        let config = Config::parse(&path, &HashMap::new()).expect("the fixture should compile");
        let payloads = control_payloads(&config);
        (dir, payloads)
    }

    /// Nothing anywhere in the object names a file: acquisition is the half a running service
    /// does not have.
    #[test]
    fn no_payload_names_a_file() {
        let (_dir, payloads) = every_key();
        let text = serde_json::to_string(&payloads).unwrap();
        assert!(!text.contains("parquet"), "{text}");
        // The four kinds this emitter compiles by hand. A layer body is `LayerDeclaration` as it
        // has always been, and its `artifact_visibility.field` is a key of that route rather than
        // an acquisition key of this one.
        for kind in ["attributes", "vocabularies", "views", "view_groups"] {
            let text = serde_json::to_string(&payloads[kind]).unwrap();
            for key in ["field", "entity_id_field", "fields", "source"] {
                assert!(
                    !text.contains(&format!("\"{key}\"")),
                    "the acquisition key `{key}` reached a {kind} payload: {text}"
                );
            }
        }
    }

    /// **Layers**: the array key and the bodies are exactly what they were — a `[[layer]]` block
    /// minus its acquisition keys is the `PUT /control/layers` body.
    #[test]
    fn layer_payloads_are_the_registration_bodies() {
        let (_dir, payloads) = every_key();
        let layers: Vec<tessera_types::layer::LayerDeclaration> =
            serde_json::from_value(payloads["layers"].clone()).expect("layer bodies");
        assert_eq!(layers.len(), 1);
        assert_eq!(layers[0].name, "clusters/a");
        assert_eq!(layers[0].views, vec!["world".to_string()]);
    }

    /// **Attributes**: declaration order over both halves of the schema, a category spelled as the
    /// block spells it, and the analyser as its declared name rather than this binary's resolved
    /// identity.
    #[test]
    fn attribute_payloads_are_the_declaration_bodies() {
        let (_dir, payloads) = every_key();
        let attributes = payloads["attributes"].as_array().unwrap();
        let names: Vec<&str> = attributes
            .iter()
            .map(|a| a["name"].as_str().unwrap())
            .collect();
        assert_eq!(
            names,
            vec!["importance", "feature", "note", "coverage"],
            "declaration order, the group-scoped column in its own place"
        );

        assert_eq!(attributes[0]["type"], "u32");
        assert_eq!(attributes[0]["title"], "Importance");
        assert_eq!(attributes[0]["index"], true);
        // **Emitted although the route refuses it** (decision 0136's amendment): the emitter
        // states the declaration and the route decides.
        assert_eq!(attributes[0]["render"], true);
        assert_eq!(attributes[0]["scope"], "entity");

        assert_eq!(attributes[1]["type"], "category");
        assert_eq!(attributes[1]["vocabulary"], "kind");
        assert_eq!(attributes[1]["width"], "u8");

        assert_eq!(attributes[2]["type"], "text");
        assert_eq!(
            attributes[2]["analyser"], "unicode",
            "the declared name, not `unicode/<version>`"
        );
        assert!(attributes[2].get("vocabulary").is_none());

        assert_eq!(attributes[3]["type"], "f32");
        assert_eq!(
            attributes[3]["scope"],
            serde_json::json!({ "group": "quarter" })
        );
    }

    /// **Vocabularies**: the declaration body, the values page beside it, and never a code.
    #[test]
    fn vocabulary_payloads_carry_values_but_no_codes() {
        let (_dir, payloads) = every_key();
        let vocabularies = payloads["vocabularies"].as_array().unwrap();
        assert_eq!(vocabularies.len(), 2);

        let kind = &vocabularies[0];
        assert_eq!(kind["name"], "kind");
        assert_eq!(kind["body"]["title"], "Feature kind");
        assert_eq!(kind["body"]["value_set"], "closed");
        assert_eq!(kind["body"]["visibility"], "public");
        assert_eq!(kind["body"]["width"], "u8");
        assert_eq!(kind["body"]["reserved"], serde_json::json!([9]));
        // **On the body and beside it**: a closed set with no values is refused at the route, so
        // the values travel with the declaration that needs them, and the page is how a value set
        // grows afterwards.
        assert_eq!(
            kind["body"]["values"],
            serde_json::json!([{ "key": "alpha" }, { "key": "beta" }]),
            "the pinned codes are the server's to draw again"
        );
        assert_eq!(
            kind["values"],
            serde_json::json!({ "values": [{ "key": "alpha" }, { "key": "beta" }] })
        );

        let region = &vocabularies[1];
        assert_eq!(region["name"], "region");
        assert_eq!(region["body"]["visibility"], "derived");
        assert_eq!(region["body"]["width"], "u16");
        assert!(
            region.get("values").is_none(),
            "a sourced value set's keys are rows, not declaration"
        );
        assert_eq!(region["values_source"], "regions");
    }

    /// **Views**: the frame in the coordinates the route takes — a projected view's degree box
    /// snapped by the routine the build snaps with, so a view declared at a running service lands
    /// where a build of the same declaration would have put it.
    #[test]
    fn view_payloads_carry_the_resolved_frame() {
        let (_dir, payloads) = every_key();
        let views = payloads["views"].as_array().unwrap();
        assert_eq!(views.len(), 2);

        assert_eq!(views[0]["name"], "world");
        assert_eq!(views[0]["body"]["title"], "Whole corpus");
        assert_eq!(views[0]["body"]["projection"], "web_mercator");
        assert_eq!(
            views[0]["body"]["visibility"],
            serde_json::json!(["ir:analyst"])
        );
        assert_eq!(
            views[0]["body"]["point_visibility"],
            serde_json::json!({ "default": "public" }),
            "`field` is where a build reads a point's own label and does not cross"
        );
        let asked = LonLatBox {
            lon_min: -180.0,
            lon_max: 180.0,
            lat_min: -85.0511287798066,
            lat_max: 85.0511287798066,
        };
        let snapped = snap_lon_lat(Projection::WebMercator, &asked)
            .square
            .bounds();
        assert_eq!(
            views[0]["body"]["extent"],
            serde_json::json!({
                "x": [snapped.x_min, snapped.x_max],
                "y": [snapped.y_min, snapped.y_max],
            })
        );

        assert_eq!(views[1]["name"], "embedding");
        assert_eq!(views[1]["body"]["projection"], "none");
        assert_eq!(
            views[1]["body"]["extent"],
            serde_json::json!({ "x": [0.0, 1000.0], "y": [0.0, 1000.0] })
        );
        assert!(views[1]["body"].get("title").is_none());
    }

    /// **View groups**: the settings its views share, and none of the roster — a group declared at
    /// a running service starts empty and its keys are created one at a time.
    #[test]
    fn view_group_payloads_drop_the_roster() {
        let (_dir, payloads) = every_key();
        let groups = payloads["view_groups"].as_array().unwrap();
        assert_eq!(groups.len(), 1);
        assert_eq!(groups[0]["name"], "quarter");
        let body = &groups[0]["body"];
        assert_eq!(body["title"], "By quarter");
        assert_eq!(body["projection"], "none");
        assert_eq!(
            body["extent"],
            serde_json::json!({ "x": [-40.0, 40.0], "y": [-40.0, 40.0] })
        );
        assert_eq!(body["visibility"], serde_json::json!(["ir:analyst"]));
        assert_eq!(
            body["point_visibility"],
            serde_json::json!({ "default": "public" })
        );
        assert_eq!(
            body["metadata"],
            serde_json::json!([
                { "name": "label", "type": "text" },
                { "name": "region", "type": "category", "vocabulary": "region" },
                { "name": "starts", "type": "timestamp_us" },
            ]),
            "the declared names, in the order the group carries them"
        );
        assert!(body.get("members").is_none(), "this group owns its views");
    }

    /// **`auto` has nothing to resolve against**, so it is emitted as declared and the route
    /// refuses it naming the key. Guessing a box would give a view a frame nobody chose, and a
    /// frame is immutable for the view's life.
    #[test]
    fn an_auto_extent_is_emitted_as_declared() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("config.toml");
        std::fs::write(
            &path,
            r#"
[[view]]
name             = "s0"
extent           = "auto"
point_visibility = { default = "public" }
"#,
        )
        .unwrap();
        let config = Config::parse(&path, &HashMap::new()).unwrap();
        let payloads = control_payloads(&config);
        assert_eq!(
            payloads["views"][0]["body"]["extent"],
            serde_json::json!({ "auto": true, "margin": DEFAULT_AUTO_MARGIN })
        );
    }
}
