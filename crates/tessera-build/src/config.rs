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
//! ## What this module does not read
//!
//! ⊘ **Acquisition is not here yet.** `source`, `fields`, a layer's inline `artifacts` and an
//! attribute's `field` are `configuration.md` §7's, bound by `--file KEY=PATH`; the build still
//! acquires through `--points`, `--pairs`, `--values`, `--artifacts` and `--artifact-members`. Each
//! of those keys is therefore **refused at parse rather than accepted and ignored**: a `source` that
//! binds nothing is a file an author believes is being read.

use std::collections::{BTreeMap, BTreeSet, HashMap, HashSet};
use std::path::{Path, PathBuf};

use serde::Deserialize;
use tessera_spatial::tiler::ScalarType;
use tessera_store::vocabulary::VocabularyMinter;
use tessera_types::layer::{
    ArtifactVisibility, ContentDeclaration, ExistenceCriterion, Hierarchy, HierarchyKind,
    LayerDeclaration, LevelDeclaration, MemberDefault, MembershipSource, SuppliedContent,
    SuppliedRequirement,
};

use crate::error::{BuildError, Result};

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

// ---------------------------------------------------------------------------------------------
// The file, as written
// ---------------------------------------------------------------------------------------------

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct ConfigFile {
    #[serde(default)]
    corpus: Option<CorpusBlock>,
    #[serde(default)]
    view: Vec<ViewBlock>,
    #[serde(default)]
    vocabulary: Vec<VocabularyBlock>,
    #[serde(default)]
    attribute: Vec<AttributeBlock>,
    #[serde(default)]
    layer: Vec<LayerBlock>,
}

/// `[corpus]` — entity space: identity and attributes, shared by every view.
///
/// Both its keys are acquisition, so the block is legal and empty this stage; the two are parsed
/// only so their refusal can name what replaces them.
#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct CorpusBlock {
    #[serde(default)]
    source: Option<String>,
    #[serde(default)]
    fields: Option<BTreeMap<String, String>>,
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
    #[serde(default)]
    point_visibility: Option<MemberVisibilityBlock>,
    #[serde(default)]
    visibility: Option<String>,
}

/// `{ field, default }` — where each member's own label is, and what one carrying none gets.
///
/// **The presence of `field` is the declaration that members carry their own labels** (C27), which
/// is why it is one table rather than a flag beside a fallback: the two cannot be declared apart.
#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct MemberVisibilityBlock {
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

/// `[[attribute]]` — one per-point column, read from `[corpus]`'s source.
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
#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct LayerBlock {
    name: String,
    #[serde(default)]
    title: Option<String>,
    #[serde(default)]
    views: Option<Vec<String>>,
    #[serde(default)]
    source: Option<String>,
    #[serde(default)]
    fields: Option<BTreeMap<String, String>>,
    #[serde(default)]
    artifacts: Option<toml::Value>,
    #[serde(default)]
    membership: Option<String>,
    #[serde(default)]
    hierarchy: Option<HierarchyBlock>,
    #[serde(default)]
    visibility: Option<String>,
    #[serde(default)]
    artifact_visibility: Option<MemberVisibilityBlock>,
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
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct HierarchyBlock {
    #[serde(default)]
    kind: Option<String>,
    /// A rendering default and the one key here carrying no disclosure argument in either
    /// direction: every artifact served has passed its own test independently.
    #[serde(default)]
    prune_children: bool,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct LevelBlock {
    #[serde(default)]
    level: Option<u32>,
    #[serde(default)]
    title: Option<String>,
    #[serde(default)]
    zoom: Option<(u32, u32)>,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct ContentBlock {
    #[serde(default)]
    computed: Vec<String>,
    #[serde(default)]
    supplied: Vec<SuppliedBlock>,
    #[serde(default)]
    withdraw_on_member_deletion: Option<bool>,
}

#[derive(Debug, Deserialize)]
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
    pub views: Vec<View>,
    /// In declaration order, which is registration order: a layer must be declared after every
    /// layer it names in `depends_on`.
    pub layers: Vec<LayerDeclaration>,
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
    /// Where each point's own access label is, and what a point carrying none gets. Recorded and
    /// ⊘ not yet read: the label column lands with acquisition (`configuration.md` §7), and today
    /// the access relation arrives through `--pairs`.
    pub point_visibility: PointVisibility,
}

/// A view's `point_visibility = { field, default }`.
#[derive(Debug, Clone)]
pub struct PointVisibility {
    pub field: Option<String>,
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

/// Whether an unknown key at ingest is refused or minted (`per-point-attributes.md` §3.4).
///
/// **Closed**: an unknown key at build (or ingest) is refused — declare-then-use, because a
/// category carries properties and, through its postings, a visibility consequence, so a typo must
/// not create one.
///
/// **Open**: an unknown key is minted a fresh code, drawn at random from the declared width's
/// unused space by [`tessera_store::vocabulary::VocabularyMinter`] — the same routine ingest uses,
/// so exhaustion is one predicate. Declared values still pin or assign codes exactly as a closed
/// vocabulary's do; the build mints only for keys the declaration does not carry, and an open
/// vocabulary given no values at all is legal and starts empty.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ValueSet {
    Closed,
    Open,
}

/// A named value set: keys, their codes, and per-value presentation.
#[derive(Debug, Clone)]
pub struct Vocabulary {
    pub name: String,
    /// ⊘ Recorded and not yet published — see [`View::title`]. A **value's** title is published,
    /// as `MANIFEST.vocabularies[..].values[..].label`.
    pub title: Option<String>,
    pub value_set: ValueSet,
    /// `public` or `derived` (`per-point-attributes.md` §3.8). Recorded and published; **not yet
    /// enforced anywhere**, there being no `/v1/categories` to filter.
    pub visibility: Listing,
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
/// place for `derived` to become `public` in translation. Its variants keep the manifest's older
/// spelling (`PerViewer` is the config's `derived`); what §1 closes is the **config's** word set,
/// and renaming a manifest discriminant buys nothing a reader of this module can see.
pub use tessera_store::manifest::Listing;

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
    /// Parse `path`, with `values` binding each closed vocabulary that sources its values from a
    /// file, by **name**.
    ///
    /// ⊘ Binding by name is the stage's stand-in for `source` + `--file KEY=PATH`
    /// (`configuration.md` §7): a vocabulary's name is already its identity, so `--values
    /// severity=…` names the same object `source = "severity"` will. The three fail-closed rules
    /// are the ones §7 states, minus the one that needs `source` to exist: a bound key no
    /// vocabulary declares is an error, and an unbound vocabulary is **never** a silent
    /// fall-through to minting.
    pub fn parse(path: &Path, values: &HashMap<String, PathBuf>) -> Result<Config> {
        let text = std::fs::read_to_string(path).map_err(|e| BuildError::io(path, e))?;
        let file: ConfigFile = toml::from_str(&text)
            .map_err(|e| declaration_error(format!("{}: {e}", path.display())))?;

        if let Some(corpus) = &file.corpus {
            refuse_acquisition("[corpus]", "source", corpus.source.is_some())?;
            refuse_acquisition("[corpus]", "fields", corpus.fields.is_some())?;
        }

        let views = compile_views(&file.view)?;
        let vocabularies = compile_vocabularies(&file.vocabulary, values)?;
        let attributes = compile_attributes(&file.attribute, &vocabularies)?;
        let layers = compile_layers(&file.layer, &views)?;

        Ok(Config {
            schema: Schema {
                attributes,
                vocabularies,
            },
            views,
            layers,
        })
    }
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

/// ⊘ Refuse one acquisition key, naming what does the job today and what will replace it.
///
/// Accepting it and reading nothing is the failure this whole surface exists to prevent: a
/// `source` that binds no file is a build silently reading none, with the author's declaration
/// sitting in the config saying otherwise (decision 0013).
fn refuse_acquisition(block: &str, key: &str, present: bool) -> Result<()> {
    if !present {
        return Ok(());
    }
    Err(declaration_error(format!(
        "{block}: `{key}` is specified and not built (configuration.md §7 — a source names a \
         logical key and `--file KEY=PATH` binds it). Acquisition still runs on `--points`, \
         `--pairs`, `--values KEY=PATH`, `--artifacts` and `--artifact-members`, so a `{key}` here \
         would name a file nothing opens. Refused rather than ignored: an unread source is one its \
         author believes is being read"
    )))
}

// ---------------------------------------------------------------------------------------------
// Views
// ---------------------------------------------------------------------------------------------

fn compile_views(blocks: &[ViewBlock]) -> Result<Vec<View>> {
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
        refuse_acquisition("[[view]]", "source", block.source.is_some())?;
        refuse_acquisition("[[view]]", "fields", block.fields.is_some())?;
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
            point_visibility: PointVisibility {
                field: point.field.clone(),
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
    values: &HashMap<String, PathBuf>,
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
        refuse_acquisition("[[vocabulary]]", "source", block.source.is_some())?;
        refuse_acquisition("[[vocabulary]]", "fields", block.fields.is_some())?;

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
            Some("public") => Listing::Public,
            Some("derived") => Listing::PerViewer,
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
        if visibility == Listing::Public && value_set == ValueSet::Open {
            eprintln!(
                "warning: vocabulary '{}': `visibility = \"public\"` with `value_set = \"open\"` \
                 publishes data-derived value names on nobody's authority (per-point-attributes \
                 §3.8, relaxed from a refusal to a warning by owner ruling 2026-08-07). Confirm \
                 this is intended",
                block.name
            );
        }

        let reserved = compile_reserved(block)?;
        let mut declared = match (&block.values, values.get(&block.name)) {
            (Some(_), Some(_)) => {
                return Err(declaration_error(format!(
                    "vocabulary '{}' declares values inline and is also bound to a file by \
                     `--values`. They are spellings of one thing, so declaring both is a parse \
                     error rather than a precedence question",
                    block.name
                )));
            }
            (Some(inline), None) => parse_inline_values(inline, &block.name)?,
            (None, Some(path)) => crate::input::read_vocabulary_file(path, &block.name)?,
            (None, None) if value_set == ValueSet::Closed => {
                return Err(declaration_error(format!(
                    "vocabulary '{}': `value_set = \"closed\"` with no value source. A closed set \
                     is authored, and an authored set of nothing refuses every ingest and costs \
                     its width in every row for ever. Declare `values = [\"a\", \"b\"]` (codes \
                     assigned in the order given), or a `[vocabulary.values]` table pinning them, \
                     or bind a file with `--values {}=<path>`. An unbound source is never a silent \
                     fall-through to minting, which would open the set with nobody deciding to",
                    block.name, block.name
                )));
            }
            // Open, and no values given: starts empty rather than closing the set, and the build
            // mints every code it will ever carry.
            (None, None) => DeclaredValues::default(),
        };

        assign_codes(&mut declared, &reserved, width, &block.name)?;
        check_codes(&declared.codes, &reserved, width, &block.name)?;

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

    // Every bound `--values` key must be claimed by some vocabulary, or the unbound-source rule
    // above merely relocates the typo (configuration.md §7).
    for key in values.keys() {
        if !compiled.contains_key(key) {
            return Err(declaration_error(format!(
                "--values bound the key '{key}', which no `[[vocabulary]]` block declares as its \
                 name. Refused rather than ignored: a typo in a binding would otherwise leave the \
                 intended vocabulary unbound and fail elsewhere"
            )));
        }
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
        refuse_acquisition("[[attribute]]", "field", decl.field.is_some())?;
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

fn compile_layers(blocks: &[LayerBlock], views: &[View]) -> Result<Vec<LayerDeclaration>> {
    let mut layers = Vec::with_capacity(blocks.len());
    let mut seen: HashSet<&str> = HashSet::new();
    for block in blocks {
        if !seen.insert(block.name.as_str()) {
            return Err(declaration_error(format!(
                "layer '{}' is declared twice. A layer name is tombstoned on drop and never \
                 reused, since bookmarks, edges and suppressions all travel by it",
                block.name
            )));
        }
        refuse_acquisition("[[layer]]", "source", block.source.is_some())?;
        refuse_acquisition("[[layer]]", "fields", block.fields.is_some())?;
        refuse_acquisition("[[layer]]", "artifacts", block.artifacts.is_some())?;

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

        let membership = match block.membership.as_deref() {
            Some("enumerated") => MembershipSource::Enumerated,
            Some("spatial") => MembershipSource::Spatial,
            Some("attribute") => MembershipSource::Attribute,
            Some(other) => {
                return Err(declaration_error(format!(
                    "layer '{}': `membership = \"{other}\"` is none of \"enumerated\" (a stored \
                     set per artifact), \"spatial\" (a shape, decomposed at request time) or \
                     \"attribute\" (a predicate over a value column)",
                    block.name
                )));
            }
            None => {
                return Err(declaration_error(format!(
                    "layer '{}': `membership` is required — it decides what a write invalidates. \
                     \"enumerated\" is a stored set per artifact, stale between the write and the \
                     refresh; \"spatial\" is a shape decomposed at request time and never stale; \
                     \"attribute\" is a predicate over a value column, likewise",
                    block.name
                )));
            }
        };

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

        let declaration = LayerDeclaration {
            name: block.name.clone(),
            title: block.title.clone().unwrap_or_default(),
            views: declared_views.clone(),
            membership,
            visibility,
            artifact_visibility,
            require_member_visibility,
            hierarchy,
            content: compile_content(block)?,
            depends_on: block.depends_on.clone(),
            levels: compile_levels(block)?,
        };
        // **One implementation of the rules, not two.** Everything `LayerRegistry::prepare_create`
        // would refuse is refused here too, by calling the same check — so a declaration refused
        // online is refused here with the same words, at parse, before a data file is opened.
        declaration.validate().map_err(|e| {
            declaration_error(format!("layer '{}': {e}", declaration.name))
        })?;
        layers.push(declaration);
    }
    Ok(layers)
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
        let title = entry.title.clone().ok_or_else(|| {
            declaration_error(format!(
                "layer '{}': level {level} declares no `title`. The metadata endpoint publishes \
                 the zoom → level map, and a level with no name is one a client cannot label",
                block.name
            ))
        })?;
        levels.push(LevelDeclaration {
            level,
            title,
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
                    n.map(|n| Some(ExistenceCriterion::Count(n))).ok_or_else(|| {
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
                    p.map(|p| Some(ExistenceCriterion::Fraction(p))).ok_or_else(|| {
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
