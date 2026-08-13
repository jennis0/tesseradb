//! `schema.toml`: the caller declares what a field *is*, and the placement follows.
//!
//! Records §2 is the design. A declaration is a `type` and three booleans, each defaulting
//! `false`: `render = true` buys a fixed-width slot in every row of `columns.arrow`;
//! `index = true` buys the family's entity-space search structure; `multi = true` is specified
//! and not built (records §5) and is **refused at parse**, naming that, per decision 0013. A
//! declaration setting neither `render` nor `index` is legal and **blob-resident** (records §3):
//! its values belong to the per-entity record blob, answered at drill-down and offered as no
//! operand. Nothing here derives a placement from the shape of the data file: a column that
//! costs 0.93 GiB per byte per row per 10⁹ items is declared or it does not exist (§4.1).
//!
//! **This is a build input, never server config** (§4.1). It compiles into `MANIFEST.json`, and
//! the server reads the compiled form. A server holding a schema of its own could be restarted
//! against a bundle whose columns disagree, and the mismatch would surface as wrong codes rather
//! than as a startup error.
//!
//! ## Why so much of this file is refusals
//!
//! The parse rules are the design. §4.3's rule — *performance knobs default; disclosure controls
//! do not* — means a category may not silently acquire a width, a listing or an open vocabulary,
//! because each of those has a consequence nobody decided. The refusals that are not merely
//! hygiene:
//!
//! - **Code `0` in a `values` block** is the *absent* sentinel (§3.6). Accepting `low = 0` would
//!   make every value-less row a member of `low` — a wrong membership set, silently.
//! - **A key in both `reserved` and the live set.** `reserved` is Protobuf's mechanism and carries
//!   its reasoning: a retired code is never reassigned, because reusing one silently recolours
//!   history.
//! - **`listing = "public"` with `vocabulary = "discovered"`** (§3.8) is *warned about, not
//!   refused* (owner ruling, 2026-08-07, relaxing §3.8's original refusal): a discovered
//!   vocabulary's values are inferred from whatever is in the corpus, so publishing them
//!   discloses data-derived names on nobody's authority, but the operator may have a reason and
//!   there is still no `/v1/categories` for it to reach.
//! - **Disagreement with a `values_of` referent** on `listing`, `vocabulary` or `width` (§3.9),
//!   or the weaker setting governs both and the gated column's value set publishes through the
//!   published one.

use std::collections::{BTreeMap, HashMap, HashSet};
use std::path::{Path, PathBuf};

use serde::Deserialize;
use tessera_spatial::tiler::ScalarType;
use tessera_store::vocabulary::VocabularyMinter;

use crate::error::{BuildError, Result};

/// A parse or consistency failure in `schema.toml`, or in a vocabulary file bound to it.
///
/// One variant carrying a message rather than a variant per rule: every one of these is a build
/// refusal an operator reads and fixes, none is caught and branched on, and a rule added as a
/// message cannot go uncaught in a `match` somewhere else.
pub fn schema_error(detail: impl Into<String>) -> BuildError {
    BuildError::Declaration(detail.into())
}

// ---------------------------------------------------------------------------------------------
// The file, as written
// ---------------------------------------------------------------------------------------------

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct SchemaFile {
    #[serde(default)]
    attribute: Vec<AttributeDecl>,
}

/// One `[[attribute]]` block, before any rule has been applied to it.
///
/// `deny_unknown_fields` throughout: a mistyped key in a disclosure control is the one class of
/// typo that must not read as a default. `listnig = "per_viewer"` under a serde that ignores
/// unknown fields is a category whose listing is whatever the absent-field rule says, declared by
/// someone who believed they had said otherwise.
#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct AttributeDecl {
    name: String,
    #[serde(rename = "type")]
    ty: String,
    #[serde(default)]
    width: Option<String>,
    /// The three placement booleans (records §2), each defaulting `false` — the cheapest
    /// placement, made more expensive only by an explicit word. Booleans rather than the
    /// retired list-valued key: its `filter` and `inspect` entries forced two faithful copies
    /// of one value, chosen separately, and the collapse removed that double store (records
    /// §2's table is the translation). A schema still carrying the retired key refuses loudly
    /// through `deny_unknown_fields` — decision 0048's shape: replaced, not aliased.
    #[serde(default)]
    render: bool,
    #[serde(default)]
    index: bool,
    #[serde(default)]
    multi: bool,
    #[serde(default)]
    render_in: Option<Vec<String>>,
    #[serde(default)]
    vocabulary: Option<String>,
    #[serde(default)]
    listing: Option<String>,
    #[serde(default)]
    values: Option<BTreeMap<String, toml::Value>>,
    #[serde(default)]
    values_key: Option<String>,
    #[serde(default)]
    values_of: Option<String>,
}

// ---------------------------------------------------------------------------------------------
// The compiled form
// ---------------------------------------------------------------------------------------------

/// A parsed, checked schema: the attributes in declaration order, and the vocabularies they
/// reference.
///
/// Declaration order is load-bearing and not a convenience. The scalar tail is stored and read
/// back **positionally** — `columns.arrow`'s schema is the fixed columns followed by this list,
/// and `/control/ingest` builds each row's vector in declared order — so reordering the file
/// reorders the columns of every segment built after it.
#[derive(Debug, Clone, Default)]
pub struct Schema {
    pub attributes: Vec<Attribute>,
    /// Keyed by vocabulary name. Several attributes may share one (§3.9): keys, codes and
    /// properties are shared, membership is not.
    pub vocabularies: HashMap<String, Vocabulary>,
}

/// One declared attribute, with its placement derived (§2).
#[derive(Debug, Clone)]
pub struct Attribute {
    pub name: String,
    /// The declared type. For a `render` column this is the hot column's width; for an
    /// `index`-only one it is the entity-space column's.
    pub ty: ScalarType,
    /// The vocabulary this column's values are drawn from, for a category; `None` for a plain
    /// numeric attribute. Names a key in [`Schema::vocabularies`].
    pub vocabulary: Option<String>,
    /// The named vocabulary's [`VocabularyKind`], cached beside its name so the batch build can
    /// decide whether to mint without a second lookup into [`Schema::vocabularies`]. `None` iff
    /// `vocabulary` is `None`.
    pub vocabulary_kind: Option<VocabularyKind>,
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

/// Whether a vocabulary's value set is authored in full before the corpus exists, or grows as the
/// corpus is discovered (§3.4).
///
/// **Declared**: an unknown key at build (or ingest) is refused — declare-then-use, because a
/// category carries properties and, through its postings, a visibility consequence, so a typo
/// must not create one.
///
/// **Discovered**: an unknown key is minted a fresh code, drawn at random from the declared
/// width's unused space by [`tessera_store::vocabulary::VocabularyMinter`] — the same routine
/// ingest uses, so exhaustion is one predicate. A `values_key` seed or an inline
/// `[attribute.values]` block still pins codes exactly as a declared vocabulary's are (§4.4); the
/// build mints only for keys the seed does not carry, and a discovered vocabulary given no value
/// source at all is legal and starts empty.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum VocabularyKind {
    Declared,
    Discovered,
}

/// A named value set: keys, their pinned codes, and per-value presentation.
#[derive(Debug, Clone)]
pub struct Vocabulary {
    pub name: String,
    pub kind: VocabularyKind,
    /// `per_viewer` or `public` (§3.8). Recorded and published; **not yet enforced anywhere**,
    /// there being no `/v1/categories` to filter — see [`Schema::parse`]'s ⊘ note.
    pub listing: Listing,
    /// Value key → code. For a declared vocabulary, every code the author pinned. For a
    /// discovered one, whatever a `values_key` seed or inline block pinned *before* the build —
    /// the codes the build mints during the run live in the minter [`Schema::discovered_minters`]
    /// returns, not here, since this struct is the schema's compiled, static state.
    pub codes: BTreeMap<String, u32>,
    /// Per-value presentation, keyed as `codes` is. Absent for a value the author gave no
    /// properties.
    pub labels: BTreeMap<String, String>,
    /// Retired codes, never reassigned (§3.4, Protobuf's `reserved`).
    pub reserved: Vec<u32>,
}

/// A compiled value set, however it was spelt: an inline `[attribute.values]` block, a bound
/// `values_key` file, or a `values_of` reference.
///
/// **One type for both spellings**, because §4.4 calls them "two spellings of one thing" — and
/// the rules in [`check_codes`] are applied to this, after they converge, so neither spelling can
/// acquire a rule the other lacks.
#[derive(Debug, Clone, Default)]
pub struct ValueSet {
    /// Value key → pinned code. Never minted here: minting is for `vocabulary = "discovered"`.
    pub codes: BTreeMap<String, u32>,
    /// Per-value presentation, keyed as `codes` is; absent for a value given no properties.
    pub labels: BTreeMap<String, String>,
    /// Retired codes, never reassigned (§3.4).
    pub reserved: Vec<u32>,
}

/// Is the *existence* of a value sensitive (§3.8)? A disclosure control, so it never defaults.
///
/// The manifest's own type, re-exported rather than mirrored: the schema compiles straight into
/// `MANIFEST.vocabularies`, and a second spelling of a two-variant disclosure control is a second
/// place for `per_viewer` to become `public` in translation.
pub use tessera_store::manifest::Listing;

impl Vocabulary {
    /// The code for `key`, or `None` if this vocabulary does not declare it.
    ///
    /// A caller mapping ingest or build data through this must treat `None` as a refusal, not as
    /// *absent*: §5's declare-then-use rule exists because a category carries properties and,
    /// through its postings, a visibility consequence, so a typo must not create one.
    pub fn code_of(&self, key: &str) -> Option<u32> {
        self.codes.get(key).copied()
    }
}

/// The reserved *absent* code (§3.6). Excluded from every value block and from minting, so a
/// row carrying no value for a column is distinguishable from one carrying the first value.
pub const ABSENT_CODE: u32 = 0;

impl Schema {
    /// Parse `path`, with `values` binding each `values_key` to a file (§4.4).
    ///
    /// **⊘ Specified, not implemented**, refused at parse rather than accepted and ignored:
    /// `multi = true` (records §5 — the list addressing lands in its own epic, records §13).
    ///
    /// **`vocabulary = "discovered"` is built** (§3.4, §5): an attribute may declare it, and the
    /// batch build mints a code for every key its value set (if any) does not already pin, through
    /// [`tessera_store::vocabulary::VocabularyMinter`] — see [`Schema::discovered_minters`] and
    /// `input::scan_attributes`.
    ///
    /// **`listing` reaches `/v1/categories`, which is what enforces it** (contracts §3.2):
    /// `public` publishes the value set, and `per_viewer` is refused there pending §3.3's
    /// membership sets. It is required rather than defaulted because §4.3 makes absence a build
    /// error for a disclosure control, and because a bundle built without one would have to be
    /// rebuilt to acquire it.
    pub fn parse(path: &Path, values: &HashMap<String, PathBuf>) -> Result<Schema> {
        let text = std::fs::read_to_string(path).map_err(|e| BuildError::io(path, e))?;
        let file: SchemaFile =
            toml::from_str(&text).map_err(|e| schema_error(format!("{}: {e}", path.display())))?;

        let mut attributes = Vec::new();
        let mut vocabularies: HashMap<String, Vocabulary> = HashMap::new();
        let mut seen_names: HashSet<&str> = HashSet::new();
        // Which vocabulary each attribute *name* resolves to, so a `values_of` reference can be
        // checked against its referent's settings (§3.9) without a second pass over the file.
        let mut vocabulary_of_attribute: HashMap<String, String> = HashMap::new();
        let mut settings_of_attribute: HashMap<String, (Listing, ScalarType)> = HashMap::new();

        for decl in &file.attribute {
            if !seen_names.insert(decl.name.as_str()) {
                return Err(schema_error(format!(
                    "attribute '{}' is declared twice. The scalar tail is stored positionally, \
                     so two columns of one name is not a last-one-wins config question — it is \
                     two columns whose values are read back under one name",
                    decl.name
                )));
            }
            check_column_name(&decl.name)?;
            // **Column names and filter combinators share one namespace** (decision 0062). A leaf
            // in a filter expression is a column name directly — there is no wrapper object — so a
            // column called `any_of` would be ambiguous with the combinator at request time.
            // Refused at the build instead, where it is one error against one declaration rather
            // than a request that means two things.
            if matches!(decl.name.as_str(), "all_of" | "any_of" | "none_of") {
                return Err(schema_error(format!(
                    "attribute '{}': that name is a filter combinator (decision 0062), and a \
                     filter expression names columns directly, so a column may not take one. \
                     Reserved: all_of, any_of, none_of",
                    decl.name
                )));
            }
            // `render` + `multi` before bare `multi`: the first is a permanent fence (0039) and
            // the second an unbuilt stage, and a caller who set both must hear the fence — it
            // survives the epic that lifts the other refusal.
            if decl.render && decl.multi {
                return Err(schema_error(format!(
                    "attribute '{}': `render` with `multi = true` is never admissible \
                     (decision 0039) — a rendered mark has one colour, and no projection or \
                     summary of a list earns a hot column. Declare an ordinary single-valued \
                     attribute carrying the value to colour by",
                    decl.name
                )));
            }
            if decl.multi {
                return Err(schema_error(format!(
                    "attribute '{}': `multi = true` is specified and not built (records §5 — \
                     the list addressing lands with the multi-value epic, records §13). Refused \
                     rather than read as single-valued: accepting it would store one value per \
                     item under a declaration promising several",
                    decl.name
                )));
            }
            if decl.render_in.is_some() {
                return Err(schema_error(format!(
                    "attribute '{}': `render_in` is specified and not built \
                     (per-point-attributes §3.9). A per-slice hot column needs contracts §2.6 to \
                     enumerate columns per slice, which slices §53 permits and the format does \
                     not yet carry — `MANIFEST.declared_scalars` is one flat bundle-wide list. \
                     Accepting it would put the column in every slice anyway, silently, which is \
                     the opposite of what it asks for. Omit it: every slice is the current \
                     behaviour and the documented default",
                    decl.name
                )));
            }
            // Neither `render` nor `index` is not a refusal: the declaration is blob-resident
            // (records §3) — no hot-column slot, no entity-space structure, no `/v1/meta`
            // operand; the record blob holds its values and drill-down returns them. The old
            // surface refused this shape because `inspect` had nowhere to put data; the blob is
            // that place. ⊘ The blob store lands beside this surface (records §13's first
            // epic); until it does, such a column exists only as its manifest declaration
            // (`index = false`, `render = false`).

            let attribute = match decl.ty.as_str() {
                "category" => {
                    let (vocab, ty) = compile_category(
                        decl,
                        values,
                        &vocabularies,
                        &vocabulary_of_attribute,
                        &settings_of_attribute,
                    )?;
                    settings_of_attribute.insert(decl.name.clone(), (vocab.listing, ty));
                    vocabulary_of_attribute.insert(decl.name.clone(), vocab.name.clone());
                    let vocab_name = vocab.name.clone();
                    let vocab_kind = vocab.kind;
                    vocabularies.entry(vocab_name.clone()).or_insert(vocab);
                    Attribute {
                        name: decl.name.clone(),
                        ty,
                        vocabulary: Some(vocab_name),
                        vocabulary_kind: Some(vocab_kind),
                        // Every flag combination is legal for a category — a rendered category
                        // stays filterable because its entity-space structures are the constant
                        // floor, not a placement (records §4.2).
                        index: decl.index,
                        render: decl.render,
                    }
                }
                // **`utf8` is retired as a declared type, and the refusal names its two
                // successors** (records §4.3, §4.4; decision 0048 makes this a refusal rather than
                // an alias, because a silent rename would give a schema a storage layout its
                // author did not choose). It remains the *wire* type of a keyword's value and of a
                // category's key, and Arrow's `Utf8` remains what those bytes are carried as — what
                // is gone is the flat string column a schema could ask for.
                "utf8" => {
                    return Err(schema_error(format!(
                        "attribute '{}': `utf8` is retired as a declared type. A short string \
                         matched whole — an identifier, an order number, a hostname — is \
                         `keyword`, which stores a per-layer sorted dictionary and a `u32` \
                         ordinal and keeps `eq`, `in`, `prefix` and `contains` byte-exact. Prose \
                         searched by word is `text` (records-and-search §4.4), ⊘ specified but not \
                         yet built, so there is no declarable type for it today",
                        decl.name
                    )));
                }
                other => {
                    // A plain scalar: the type *is* the width, and none of the vocabulary
                    // machinery applies. Refused rather than ignored if any of it is present,
                    // because a `listing` on a non-category is a disclosure control the author
                    // believes they have set.
                    let ty = ScalarType::parse(other).ok_or_else(|| {
                        schema_error(format!(
                            "attribute '{}': unknown type '{other}'. Declarable types are \
                             bool, u8, u16, u32, u64, i8, i16, i32, i64, f32, f64, \
                             timestamp_us, keyword and category",
                            decl.name
                        ))
                    })?;
                    for (field, present) in [
                        ("width", decl.width.is_some()),
                        ("vocabulary", decl.vocabulary.is_some()),
                        ("listing", decl.listing.is_some()),
                        ("values", decl.values.is_some()),
                        ("values_key", decl.values_key.is_some()),
                        ("values_of", decl.values_of.is_some()),
                    ] {
                        if present {
                            return Err(schema_error(format!(
                                "attribute '{}' is type '{other}', not a category, so `{field}` \
                                 has no meaning for it. Refused rather than ignored: an ignored \
                                 `listing` is a disclosure control its author believes is set",
                                decl.name
                            )));
                        }
                    }
                    // **`render`, not the type.** A string is refused from the *hot column*, which
                    // is per-row and served; it is not refused from the bundle. An `index`-only
                    // keyword lives in entity space, is read once per query rather than once per
                    // rendered mark, and costs the hot column nothing — which is exactly the
                    // placement distinction §10.3 routes by.
                    //
                    // A `keyword` is fixed-width in storage — the `u32` ordinal — and is refused
                    // anyway, which is the stronger reason of the two the retired `utf8` had:
                    // rendering one would put either the value's bytes in every row, at 0.93 GiB
                    // per byte per row per 10⁹, or its ordinal, which is a position in one layer's
                    // dictionary that means nothing outside that layer and is an index internal
                    // that never crosses the trust boundary (records §4.3, **I10**). Neither is a
                    // colour a client can draw.
                    if ty == ScalarType::Keyword && decl.render {
                        return Err(schema_error(format!(
                            "attribute '{}': `render` on `keyword` is refused \
                             (per-point-attributes §4.3 — the hot column is a fixed-width slot in \
                             every row, and a keyword's value is not one). Its ordinal is \
                             fixed-width but is a per-layer index internal that never leaves the \
                             server (records §4.3). Declare a category, whose row cost is its \
                             width. `index = true` is available and costs the hot column nothing",
                            decl.name
                        )));
                    }
                    Attribute {
                        name: decl.name.clone(),
                        ty,
                        vocabulary: None,
                        vocabulary_kind: None,
                        index: decl.index,
                        render: decl.render,
                    }
                }
            };
            attributes.push(attribute);
        }

        // Every bound `--values` key must be claimed by some attribute, or the unbound-key rule
        // above merely relocates the typo (§4.4).
        let claimed: HashSet<&String> = file
            .attribute
            .iter()
            .filter_map(|d| d.values_key.as_ref())
            .collect();
        for key in values.keys() {
            if !claimed.contains(key) {
                return Err(schema_error(format!(
                    "--values bound the key '{key}', which no attribute declares as its \
                     `values_key`. Refused rather than ignored: a typo in a binding would \
                     otherwise leave the intended attribute unbound and fail elsewhere"
                )));
            }
        }

        Ok(Schema {
            attributes,
            vocabularies,
        })
    }

    /// **Bits** this schema adds to every row, and `None` if any counted column is
    /// variable-width.
    ///
    /// §2.3's residency figure, **totalled across attributes rather than reported per column** —
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
    /// before `--schema` existed, which must stay buildable and byte-identical.
    pub fn is_empty(&self) -> bool {
        self.attributes.is_empty()
    }

    /// One live [`VocabularyMinter`] per discovered vocabulary this schema declares, seeded from
    /// whatever it already pins — a `values_key` seed's or inline block's codes, plus `reserved`
    /// (§4.4: seeding pins codes exactly as a declared vocabulary's are, and the build mints only
    /// for keys the seed does not carry). A declared vocabulary mints nothing and has no entry
    /// here at all, so `input::scan_attributes`'s batch-level mint pre-pass can never reach one.
    ///
    /// The width bounding a vocabulary's draw is taken from the **column** that declares it, not
    /// from the vocabulary itself — the same rule
    /// `tessera_store::vocabulary::Vocabularies::seed` states for the served bundle: a value set
    /// is keys and codes, and what bounds the code space is what stores it.
    pub fn discovered_minters(&self) -> HashMap<String, VocabularyMinter> {
        let mut minters = HashMap::new();
        for vocabulary in self.vocabularies.values() {
            if vocabulary.kind != VocabularyKind::Discovered {
                continue;
            }
            let width = self
                .attributes
                .iter()
                .find(|a| a.vocabulary.as_deref() == Some(vocabulary.name.as_str()))
                .map(|a| a.ty)
                .expect("a compiled vocabulary is named by at least one attribute");
            let mut minter = VocabularyMinter::new(
                vocabulary.name.clone(),
                tessera_store::manifest::VocabularyKind::Discovered,
                vocabulary.listing,
                width,
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

/// Compile one `type = "category"` block into its vocabulary and its column width.
fn compile_category(
    decl: &AttributeDecl,
    values: &HashMap<String, PathBuf>,
    vocabularies: &HashMap<String, Vocabulary>,
    vocabulary_of_attribute: &HashMap<String, String>,
    settings_of_attribute: &HashMap<String, (Listing, ScalarType)>,
) -> Result<(Vocabulary, ScalarType)> {
    // ---- width: required, no default, and unalterable (§3.6, §2.2) -------------------------
    let width_name = decl.width.as_deref().ok_or_else(|| {
        schema_error(format!(
            "attribute '{}': `width` is required for a category and has no default \
             (per-point-attributes §4.3). It is baked into every row, so changing it rewrites \
             the corpus — an unsupported migration, not a default worth guessing",
            decl.name
        ))
    })?;
    let ty = ScalarType::parse(width_name)
        .filter(|t| t.is_category_width())
        .ok_or_else(|| {
            schema_error(format!(
                "attribute '{}': `width = \"{width_name}\"` is not a category width. The three \
                 are u8 (255 usable values), u16 and u32 — per-point-attributes §3.6. Code 0 is \
                 the reserved *absent* sentinel, which is why each carries one fewer value than \
                 its range",
                decl.name
            ))
        })?;

    // ---- the value set: exactly one of three spellings (§4.4) -------------------------------
    let sources = [
        ("[attribute.values]", decl.values.is_some()),
        ("values_key", decl.values_key.is_some()),
        ("values_of", decl.values_of.is_some()),
    ];
    let given: Vec<&str> = sources
        .iter()
        .filter(|(_, present)| *present)
        .map(|(name, _)| *name)
        .collect();
    if given.len() > 1 {
        return Err(schema_error(format!(
            "attribute '{}' declares {} together. They are spellings of one thing, so declaring \
             more than one is a parse error rather than a precedence question \
             (per-point-attributes §4.4)",
            decl.name,
            given.join(" and ")
        )));
    }

    // ---- `values_of`: share a referent's keys, codes and properties (§3.9) ------------------
    if let Some(referent) = &decl.values_of {
        let vocab_name = vocabulary_of_attribute.get(referent).ok_or_else(|| {
            schema_error(format!(
                "attribute '{}': `values_of = \"{referent}\"` names no attribute declared before \
                 it. The reference is resolved in file order so that a cycle cannot be written",
                decl.name
            ))
        })?;
        let vocab = vocabularies
            .get(vocab_name)
            .expect("a resolved attribute has a compiled vocabulary");
        let (referent_listing, referent_ty) = settings_of_attribute
            .get(referent)
            .expect("a resolved attribute has recorded settings");
        // §3.9: disagreement here means the weaker setting governs both, and the gated column's
        // value set publishes through the published one.
        if let Some(listing) = &decl.listing {
            let mine = parse_listing(listing, &decl.name)?;
            if mine != *referent_listing {
                return Err(schema_error(format!(
                    "attribute '{}' shares '{referent}'s vocabulary but declares \
                     `listing = \"{}\"` against its \"{}\". Attributes sharing a vocabulary must \
                     agree, or the weaker setting governs both and the gated column's value set \
                     publishes through the published one (per-point-attributes §3.9)",
                    decl.name,
                    mine.as_str(),
                    referent_listing.as_str()
                )));
            }
        }
        if ty != *referent_ty {
            return Err(schema_error(format!(
                "attribute '{}' shares '{referent}'s vocabulary but declares \
                 `width = \"{}\"` against its \"{}\". A shared vocabulary is one code space; two \
                 widths over it is one column unable to hold the other's codes \
                 (per-point-attributes §3.9)",
                decl.name,
                ty.arrow_type_name(),
                referent_ty.arrow_type_name()
            )));
        }
        return Ok((vocab.clone(), ty));
    }

    // ---- `vocabulary`: required, and it decides whether `public` is legal (§4.3) ------------
    let vocabulary_kind = decl.vocabulary.as_deref().ok_or_else(|| {
        schema_error(format!(
            "attribute '{}': `vocabulary` is required for a category and has no default \
             (per-point-attributes §4.3). It decides whether `listing = \"public\"` is even \
             legal, and a safe default there is a decision nobody made",
            decl.name
        ))
    })?;
    let kind = match vocabulary_kind {
        "declared" => VocabularyKind::Declared,
        "discovered" => VocabularyKind::Discovered,
        other => {
            return Err(schema_error(format!(
                "attribute '{}': `vocabulary = \"{other}\"` is neither \"declared\" nor \
                 \"discovered\"",
                decl.name
            )));
        }
    };

    let listing_name = decl.listing.as_deref().ok_or_else(|| {
        schema_error(format!(
            "attribute '{}': `listing` is required for a category and has no default \
             (per-point-attributes §4.3). It is a disclosure control — whether the *existence* \
             of a value is sensitive — so its absence is a build error exactly as `[disclosure]`'s \
             absence is a startup error",
            decl.name
        ))
    })?;
    let listing = parse_listing(listing_name, &decl.name)?;
    // §3.8's original refusal, relaxed to a warning by owner ruling (2026-08-07): a discovered
    // vocabulary's values are inferred from whatever is in the corpus, so publishing them
    // discloses data-derived names on nobody's authority — but the operator may have a reason,
    // and there is still no `/v1/categories` for the disclosure to reach. Surfaced through the
    // same `eprintln!("warning: ...")` channel the batch build already uses for its other
    // build-time warnings (see `lib.rs`/`pipeline.rs`'s `over_bound_items`), rather than a new
    // mechanism.
    if listing == Listing::Public && kind == VocabularyKind::Discovered {
        eprintln!(
            "warning: attribute '{}': `listing = \"public\"` with `vocabulary = \"discovered\"` \
             publishes data-derived value names on nobody's authority (per-point-attributes \
             §3.8, relaxed from a refusal to a warning by owner ruling 2026-08-07). Confirm this \
             is intended",
            decl.name
        );
    }

    let ValueSet {
        codes,
        labels,
        reserved,
    } = if let Some(values) = &decl.values {
        parse_inline_values(values, &decl.name)?
    } else if let Some(key) = &decl.values_key {
        let path = values.get(key).ok_or_else(|| {
            schema_error(format!(
                "attribute '{}': `values_key = \"{key}\"` is not bound. Pass \
                 `--values {key}=<path>`. An unbound key is a build failure and never a silent \
                 fall-through to auto-mint, which would convert a closed vocabulary to an open \
                 one without anyone deciding to (per-point-attributes §4.4)",
                decl.name
            ))
        })?;
        crate::input::read_vocabulary_file(path, &decl.name)?
    } else if kind == VocabularyKind::Declared {
        return Err(schema_error(format!(
            "attribute '{}': `vocabulary = \"declared\"` needs its value set — one of an inline \
             `[attribute.values]` block, a `values_key` bound at build, or a `values_of` \
             reference (per-point-attributes §4.4)",
            decl.name
        )));
    } else {
        // Discovered, and no `values`/`values_key` given (§4.4): starts empty rather than
        // closing the set, and the build mints every code it will ever carry.
        ValueSet::default()
    };

    check_codes(&codes, &reserved, ty, &decl.name, kind)?;

    Ok((
        Vocabulary {
            // A vocabulary declared inline is named for its attribute; one from a file keeps the
            // logical key, so two attributes binding the same key share one compiled vocabulary.
            name: decl.values_key.clone().unwrap_or_else(|| decl.name.clone()),
            kind,
            listing,
            codes,
            labels,
            reserved,
        },
        ty,
    ))
}

fn parse_listing(name: &str, attribute: &str) -> Result<Listing> {
    match name {
        "per_viewer" => Ok(Listing::PerViewer),
        "public" => Ok(Listing::Public),
        other => Err(schema_error(format!(
            "attribute '{attribute}': `listing = \"{other}\"` is neither \"per_viewer\" nor \
             \"public\" (per-point-attributes §3.8)"
        ))),
    }
}

/// An inline `[attribute.values]` block: `key = code`, plus `reserved = [...]`.
///
/// Codes only — a gate label or a colour is per-value data belonging in the file form (§3.8), and
/// the inline block deliberately cannot express it.
fn parse_inline_values(
    values: &BTreeMap<String, toml::Value>,
    attribute: &str,
) -> Result<ValueSet> {
    let mut set = ValueSet::default();
    for (key, value) in values {
        if key == "reserved" {
            let list = value.as_array().ok_or_else(|| {
                schema_error(format!(
                    "attribute '{attribute}': `reserved` must be an array of codes"
                ))
            })?;
            for entry in list {
                set.reserved.push(as_code(entry, attribute, "reserved")?);
            }
            continue;
        }
        set.codes
            .insert(key.clone(), as_code(value, attribute, key)?);
    }
    Ok(set)
}

fn as_code(value: &toml::Value, attribute: &str, key: &str) -> Result<u32> {
    let raw = value.as_integer().ok_or_else(|| {
        schema_error(format!(
            "attribute '{attribute}': value '{key}' must be an integer code, not {value}"
        ))
    })?;
    u32::try_from(raw).map_err(|_| {
        schema_error(format!(
            "attribute '{attribute}': value '{key}' has code {raw}, which is not a u32"
        ))
    })
}

/// The rules a compiled code set must satisfy, whatever spelling it arrived in.
///
/// Applied after the inline and file forms converge, so neither can acquire a rule the other
/// lacks — the two spellings being "two spellings of one thing" (§4.4) in the checks as well as
/// in the parse.
fn check_codes(
    codes: &BTreeMap<String, u32>,
    reserved: &[u32],
    ty: ScalarType,
    attribute: &str,
    kind: VocabularyKind,
) -> Result<()> {
    // A *declared* vocabulary with no values would cost its width for nothing (every row carries
    // the absent sentinel forever). A *discovered* one starting empty is the documented default
    // (§4.4) — the build mints its first code the moment the corpus supplies one.
    if codes.is_empty() && kind == VocabularyKind::Declared {
        return Err(schema_error(format!(
            "attribute '{attribute}': a declared vocabulary with no values. Every row would \
             carry the absent sentinel and the column would cost its width for nothing"
        )));
    }
    let max = ty
        .max_code()
        .expect("a category width always has a maximum code");
    let mut seen: HashMap<u32, &str> = HashMap::new();
    let reserved_set: HashSet<u32> = reserved.iter().copied().collect();
    for (key, &code) in codes {
        if code == ABSENT_CODE {
            return Err(schema_error(format!(
                "attribute '{attribute}': value '{key}' is assigned code 0, which is the \
                 reserved *absent* sentinel (per-point-attributes §3.6). Code 0 is what \
                 preserves `columns.arrow`'s contractual non-nullability without a validity \
                 buffer, so '{key} = 0' would make every value-less row a member of '{key}'"
            )));
        }
        if code > max {
            return Err(schema_error(format!(
                "attribute '{attribute}': value '{key}' has code {code}, past `{}`'s maximum \
                 of {max}. Never widen and never wrap — the remedy is a rebuild at a wider \
                 declared width (per-point-attributes §3.6)",
                ty.arrow_type_name()
            )));
        }
        if reserved_set.contains(&code) {
            return Err(schema_error(format!(
                "attribute '{attribute}': value '{key}' has code {code}, which is also listed \
                 `reserved`. A retired code is never reassigned: reusing one silently recolours \
                 every row that carried it (per-point-attributes §3.4)"
            )));
        }
        if let Some(other) = seen.insert(code, key) {
            return Err(schema_error(format!(
                "attribute '{attribute}': values '{other}' and '{key}' share code {code}. The \
                 row stores only the code, so two keys at one code are one colour under two \
                 names and no way back"
            )));
        }
    }
    Ok(())
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
        return Err(schema_error("an attribute with an empty name"));
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
        return Err(schema_error(format!(
            "attribute '{name}': a column name is its identifier on the wire \
             (`/v1/categories/{{column}}`, contracts §3.2), so it is limited to ASCII letters, \
             digits, `_` and `-`"
        )));
    }
    if FIXED.contains(&name) {
        return Err(schema_error(format!(
            "attribute '{name}' shadows a fixed column of `columns.arrow` (contracts §2.6). The \
             reader refuses such a segment at load"
        )));
    }
    if name == "record" {
        return Err(schema_error(
            "attribute 'record': the name is reserved — `attrs/record/` is the record blob's \
             namespace (records §2, review N10), so a column of that name would address the \
             blob's files as its own",
        ));
    }
    if INGEST_RESERVED.contains(&name) {
        return Err(schema_error(format!(
            "attribute '{name}' shadows a reserved `/control/ingest` column (contracts §3.4), so \
             no batch could ever carry a value for it — the handler would read the reserved \
             column's meaning instead"
        )));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Parse `text` as a schema file, through a private temporary directory.
    ///
    /// **A fresh `tempdir` per call, not a name derived from the input.** An earlier version named
    /// the file after the string's heap address (`{:p}`), which the allocator reuses: two cases
    /// running in parallel could land on one path, and a case could read the file another had
    /// written. It presented as a *refusal that did not fire* — the parse succeeded against stale
    /// bytes — which is the most misleading way for a test helper to fail, since the assertion it
    /// breaks is the one asserting something is refused.
    fn parse_str(text: &str) -> Result<Schema> {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("schema.toml");
        std::fs::write(&path, text).expect("write schema");
        Schema::parse(&path, &HashMap::new())
    }

    fn err(text: &str) -> String {
        format!("{}", parse_str(text).expect_err("expected a refusal"))
    }

    /// `base` with `line` added to its first `[[attribute]]` table.
    ///
    /// **Inserted at a structural landmark, not by matching a formatted line.** A case that built
    /// its input with `str::replace` on a placement line silently stopped substituting when
    /// the constant's alignment changed — and a case asserting that something is *refused* then
    /// passes plain `SEVERITY`, which is accepted, so the no-op presents as the refusal failing to
    /// fire rather than as a broken fixture. Panics if the landmark is gone, which is the whole
    /// point: a fixture that cannot build its input must fail loudly, not test nothing.
    fn with_line(base: &str, line: &str) -> String {
        const AFTER: &str = "[[attribute]]\n";
        assert!(
            base.contains(AFTER),
            "the fixture no longer contains an [[attribute]] table header"
        );
        base.replacen(AFTER, &format!("{AFTER}{line}\n"), 1)
    }

    const SEVERITY: &str = r#"
[[attribute]]
name = "severity"
type = "category"
width = "u8"
render = true
vocabulary = "declared"
listing = "public"
  [attribute.values]
  low = 1
  high = 2
"#;

    #[test]
    fn a_declared_category_compiles_to_a_width_and_a_pinned_code_set() {
        let schema = parse_str(SEVERITY).unwrap();
        assert_eq!(schema.attributes.len(), 1);
        assert_eq!(schema.attributes[0].ty, ScalarType::U8);
        assert_eq!(schema.row_bits(), Some(8));
        let vocab = &schema.vocabularies["severity"];
        assert_eq!(vocab.code_of("low"), Some(1));
        assert_eq!(vocab.code_of("nonesuch"), None);
        assert_eq!(vocab.listing, Listing::Public);
    }

    /// §3.6: code 0 is the *absent* sentinel, so `low = 0` would make every value-less row a
    /// member of `low`. The one rule in this file whose violation is silent rather than loud.
    #[test]
    fn code_zero_is_refused_because_it_is_the_absent_sentinel() {
        let text = SEVERITY.replace("low = 1", "low = 0");
        assert!(err(&text).contains("absent"), "{}", err(&text));
    }

    #[test]
    fn a_code_past_the_declared_width_is_refused_rather_than_widened() {
        let text = SEVERITY.replace("high = 2", "high = 300");
        let message = err(&text);
        assert!(message.contains("maximum of 255"), "{message}");
        assert!(message.contains("rebuild"), "{message}");
    }

    #[test]
    fn two_values_at_one_code_are_refused() {
        let text = SEVERITY.replace("high = 2", "high = 1");
        assert!(err(&text).contains("share code 1"));
    }

    /// §3.4: `reserved` is a tombstone, and reassigning a retired code silently recolours every
    /// row that carried it.
    #[test]
    fn a_reserved_code_may_not_be_reassigned() {
        let text = SEVERITY.replace("high = 2", "high = 2\n  reserved = [2]");
        assert!(err(&text).contains("reserved"));
    }

    /// §4.3: the three that must not default, each for its own reason.
    #[test]
    fn width_vocabulary_and_listing_are_each_required_for_a_category() {
        for (line, expected) in [
            ("width = \"u8\"\n", "`width` is required"),
            ("vocabulary = \"declared\"\n", "`vocabulary` is required"),
            ("listing = \"public\"\n", "`listing` is required"),
        ] {
            let text = SEVERITY.replace(line, "");
            let message = err(&text);
            assert!(message.contains(expected), "{message}");
        }
    }

    /// A category may declare `index` beside `render`, and it reaches the compiled form.
    ///
    /// This is the placement's whole observable effect at parse: `Attribute::index` is what the
    /// build compiles into `MANIFEST.declared_scalars[..].index`, and a reader picks the postings
    /// record format from the declaration rather than from a stored second copy (manifest §2.5).
    /// A rendered category is the one rendered shape that may set it — its entity-space
    /// structures are the constant floor, not a placement (records §4.2) — where a rendered
    /// number is refused below.
    #[test]
    fn a_category_may_declare_index() {
        let text = SEVERITY.replace("render = true", "render = true\nindex = true");
        let schema = parse_str(&text).expect("index is built for a category");
        assert!(schema.attributes[0].index);

        let render_only = parse_str(SEVERITY).expect("render alone stays legal");
        assert!(!render_only.attributes[0].index);
    }

    /// `index` alone is a legal declaration — a column indexed for querying and never drawn.
    /// §10.3 routes by access cadence, so per-query and per-mark are independent choices.
    #[test]
    fn index_without_render_is_legal() {
        let text = SEVERITY.replace("render = true", "index = true");
        let schema = parse_str(&text).expect("index alone declares something");
        assert!(schema.attributes[0].index);
        assert!(!schema.attributes[0].vocabulary.is_none());
    }

    /// `index` alone is accepted on every declarable type — a numeric is a value column and
    /// nothing else, so there is no structure left for it to be waiting on.
    ///
    /// `index` *alone*: the fixture once declared these rendered-and-filterable, and that
    /// combination is now the refusal below (review X3's named regression) — restructured
    /// index-only here because what this case exercises is the filter placement.
    #[test]
    fn index_is_accepted_on_every_declarable_type() {
        for ty in [
            "bool",
            "u8",
            "u16",
            "u32",
            "u64",
            "i8",
            "i16",
            "i32",
            "i64",
            "f32",
            "f64",
            "timestamp_us",
        ] {
            let text = format!(
                r#"
[[attribute]]
name = "measure"
type = "{ty}"
index = true
"#
            );
            let schema = parse_str(&text).unwrap_or_else(|e| panic!("{ty} must filter: {e}"));
            assert!(schema.attributes[0].index, "{ty}");
            assert!(!schema.attributes[0].render, "{ty}");
        }
    }

    /// A number, a datetime and a bool may be rendered **and** indexed — the two homes of one
    /// column, which is what gives decision 0068 two routes to choose between on cost.
    ///
    /// This combination was refused while 0064's render half was unbuilt, because the row route
    /// would have read absence out of a hot column that stores it as the type's zero. The presence
    /// bitmap beside the column is what removes that, and the row scan honours it
    /// (`viewport.rs`'s `an_absent_number_matches_no_range_not_even_one_containing_zero`).
    #[test]
    fn a_number_may_be_rendered_and_indexed_at_once() {
        for ty in ["bool", "i32", "f64", "timestamp_us"] {
            let text = format!(
                r#"
[[attribute]]
name = "measure"
type = "{ty}"
render = true
index = true
"#
            );
            let schema =
                parse_str(&text).unwrap_or_else(|e| panic!("{ty} must render and filter: {e}"));
            assert!(schema.attributes[0].index, "{ty}");
            assert!(schema.attributes[0].render, "{ty}");
        }
    }

    /// Decision 0013: absent machinery names itself rather than refusing generically. Bare
    /// `multi` names records §5 and the epic that lifts it; `render` + `multi` names decision
    /// 0039's permanent fence instead, whatever else the declaration says.
    #[test]
    fn multi_is_refused_naming_what_is_absent_and_0039_when_rendered() {
        let multi = SEVERITY.replace("render = true", "index = true\nmulti = true");
        assert!(err(&multi).contains("records §5"), "{}", err(&multi));

        let rendered = SEVERITY.replace("render = true", "render = true\nmulti = true");
        assert!(err(&rendered).contains("0039"), "{}", err(&rendered));
    }

    /// A declaration with neither `render` nor `index` parses and is blob-resident
    /// (records §3): compiled with both flags false, occupying no row bits. The old "must
    /// contain render or filter" refusal is deleted, not reworded.
    #[test]
    fn a_declaration_with_neither_key_is_blob_resident() {
        let text = r#"
[[attribute]]
name = "band"
type = "category"
width = "u8"
render = true
vocabulary = "declared"
listing = "public"
  [attribute.values]
  low = 1

[[attribute]]
name = "notes"
type = "keyword"

[[attribute]]
name = "revision"
type = "i64"
"#;
        let schema = parse_str(text).expect("neither key declares a blob-resident column");
        for i in [1, 2] {
            assert!(!schema.attributes[i].index, "{}", schema.attributes[i].name);
            assert!(
                !schema.attributes[i].render,
                "{}",
                schema.attributes[i].name
            );
        }
        // The hot column's tail is exactly the render columns, so the blob-resident `i64` and
        // the entity-space `keyword` cost no row bits — only the rendered `u8` counts.
        assert_eq!(schema.row_bits(), Some(8));
    }

    /// `record` is reserved: `attrs/record/` is the record blob's namespace (records §2,
    /// review N10).
    #[test]
    fn record_is_a_reserved_column_name() {
        let text = SEVERITY.replace("\"severity\"", "\"record\"");
        let message = err(&text);
        assert!(message.contains("record blob"), "{message}");
    }

    /// The refusal is lifted: `vocabulary = "discovered"` compiles, with its own pinned inline
    /// value set intact (seeding still pins codes exactly as a declared vocabulary's are, §4.4).
    #[test]
    fn a_discovered_category_compiles_with_its_kind_recorded() {
        let discovered = SEVERITY.replace("\"declared\"", "\"discovered\"");
        let schema = parse_str(&discovered).unwrap();
        assert_eq!(
            schema.attributes[0].vocabulary_kind,
            Some(VocabularyKind::Discovered)
        );
        let vocab = &schema.vocabularies["severity"];
        assert_eq!(vocab.kind, VocabularyKind::Discovered);
        // The inline block still pins "low"/"high" exactly as it would for a declared vocabulary.
        assert_eq!(vocab.code_of("low"), Some(1));
    }

    /// §4.4: unlike a declared vocabulary, a discovered one with no value source at all is legal
    /// and starts empty — the build mints its first code once the corpus supplies a key.
    #[test]
    fn a_discovered_category_with_no_value_source_starts_empty() {
        let text = r#"
[[attribute]]
name = "department"
type = "category"
width = "u16"
render = true
vocabulary = "discovered"
listing = "per_viewer"
"#;
        let schema = parse_str(text).unwrap();
        let vocab = &schema.vocabularies["department"];
        assert_eq!(vocab.kind, VocabularyKind::Discovered);
        assert!(vocab.codes.is_empty());
    }

    /// A *declared* vocabulary with no value source is still a parse error — only a discovered
    /// one is allowed to start empty.
    #[test]
    fn a_declared_category_with_no_value_source_still_refuses() {
        let text = r#"
[[attribute]]
name = "department"
type = "category"
width = "u16"
render = true
vocabulary = "declared"
listing = "per_viewer"
"#;
        assert!(err(text).contains("needs its value set"), "{}", err(text));
    }

    /// §3.8's original refusal is relaxed to a warning (owner ruling 2026-08-07): the combination
    /// now builds. Asserting the exact warning text would mean capturing stderr, which is
    /// disproportionate here — this test's job is to prove the refusal is gone.
    #[test]
    fn public_listing_with_a_discovered_vocabulary_builds_rather_than_refuses() {
        // SEVERITY already declares `listing = "public"`; only the vocabulary kind changes.
        let text = SEVERITY.replace("\"declared\"", "\"discovered\"");
        let schema = parse_str(&text).unwrap();
        assert_eq!(schema.vocabularies["severity"].listing, Listing::Public);
        assert_eq!(
            schema.vocabularies["severity"].kind,
            VocabularyKind::Discovered
        );
    }

    /// **`render_in` is refused rather than recorded and ignored** (§3.9, decision 0013).
    ///
    /// The distinction is the whole point of the case. `MANIFEST.declared_scalars` is one flat
    /// bundle-wide list, so a build that accepted `render_in = ["a"]` would write the column into
    /// every slice — the opposite of what was asked for, with no error and nothing downstream able
    /// to notice. Omitting it still means every slice, which is honest because that is what
    /// happens.
    #[test]
    fn render_in_is_refused_rather_than_silently_ignored() {
        let text = with_line(SEVERITY, "render_in = [\"docs_2024\"]");
        let message = err(&text);
        assert!(message.contains("§3.9"), "{message}");
        assert!(
            message.contains("every slice anyway"),
            "the refusal must say what accepting it would actually do: {message}"
        );
        // And the omitted case is unaffected — it is the documented default, not a workaround.
        assert!(parse_str(SEVERITY).is_ok());
    }

    /// §4.3: the hot column is a fixed-width slot per row, and a keyword's value is not one. The
    /// refusal is at the declaration, not in the storage layer.
    ///
    /// The message must carry the *stronger* of the two reasons, because the weaker one is false
    /// of a keyword: its ordinal **is** fixed-width, so "not fixed-width" alone would let a reader
    /// conclude that rendering the ordinal is fine — and an ordinal is a per-layer index internal
    /// crossing the trust boundary.
    #[test]
    fn render_on_a_keyword_is_refused_at_the_declaration() {
        let message = err(r#"
[[attribute]]
name = "title"
type = "keyword"
render = true
"#);
        assert!(message.contains("fixed-width slot"), "{message}");
        assert!(message.contains("keyword"), "{message}");
        assert!(
            message.contains("never leaves the server"),
            "a keyword's refusal must name the ordinal's confinement, not only the width: \
             {message}"
        );
    }

    /// **`utf8` is refused as a declared type, and the refusal names what to declare instead.**
    ///
    /// A refusal rather than an alias for `keyword`: decision 0048 spends no effort on a past, and
    /// a silent rename would give a column a storage layout — a dictionary and an ordinal — that
    /// its author never chose. The message must reach both successors, because a schema that meant
    /// prose is not served by `keyword` and would otherwise be quietly mis-declared.
    #[test]
    fn utf8_is_refused_as_a_declared_type_and_names_its_successors() {
        let message = err(r#"
[[attribute]]
name = "title"
type = "utf8"
index = true
"#);
        assert!(message.contains("retired"), "{message}");
        assert!(message.contains("keyword"), "{message}");
        assert!(
            message.contains("text"),
            "the prose successor must be named too: {message}"
        );
    }

    /// A column may not take a combinator's name — refused at the build, not resolved at the
    /// request.
    #[test]
    fn a_column_may_not_be_named_after_a_combinator() {
        for name in ["all_of", "any_of", "none_of"] {
            let text = format!(
                r#"
[[attribute]]
name = "{name}"
type = "keyword"
index = true
"#
            );
            assert!(err(&text).contains("filter combinator"), "{}", err(&text));
        }
    }

    /// A string is refused from the **hot column**, not from the bundle: `index` puts it in
    /// entity space, where it is read once per query rather than once per rendered mark.
    #[test]
    fn an_index_only_keyword_is_accepted() {
        let schema = parse_str(
            r#"
[[attribute]]
name = "title"
type = "keyword"
index = true
"#,
        )
        .expect("an index-only keyword parses");
        assert!(schema.attributes[0].index);
        assert_eq!(schema.attributes[0].ty, ScalarType::Keyword);
        // A keyword occupies no row slot, so it does not move the residency figure.
        assert_eq!(schema.row_bits(), Some(0));
    }

    /// An unknown type names the whole declarable set, `keyword` included: the message is how an
    /// author discovers the type exists.
    #[test]
    fn the_type_list_names_keyword() {
        let text = r#"
[[attribute]]
name = "title"
type = "keywords"
index = true
"#;
        let message = err(text);
        assert!(message.contains("keyword and category"), "{message}");
        assert!(
            !message.contains("utf8"),
            "the retired type must not be advertised as declarable: {message}"
        );
    }

    #[test]
    fn a_plain_scalar_needs_none_of_the_vocabulary_machinery() {
        let text = r#"
[[attribute]]
name = "ingested_at"
type = "i64"
render = true

[[attribute]]
name = "score"
type = "f32"
render = true
"#;
        let schema = parse_str(text).unwrap();
        assert_eq!(schema.attributes[0].ty, ScalarType::I64);
        assert_eq!(schema.attributes[1].ty, ScalarType::F32);
        assert!(schema.attributes[0].vocabulary.is_none());
        assert_eq!(schema.row_bits(), Some(96));
    }

    /// A `listing` on a non-category is a disclosure control its author believes is set, so it is
    /// refused rather than ignored.
    #[test]
    fn vocabulary_fields_on_a_non_category_are_refused_rather_than_ignored() {
        let text = r#"
[[attribute]]
name = "score"
type = "f32"
render = true
listing = "public"
"#;
        assert!(err(text).contains("has no meaning"), "{}", err(text));
    }

    /// §3.9: sharing a vocabulary shares keys, codes and properties — never `listing` or `width`
    /// disagreement, which would publish the gated column's value set through the published one.
    #[test]
    fn attributes_sharing_a_vocabulary_must_agree_on_listing_and_width() {
        let base = r#"
[[attribute]]
name = "department"
type = "category"
width = "u16"
render = true
vocabulary = "declared"
listing = "per_viewer"
  [attribute.values]
  eng = 1
  finance = 2

[[attribute]]
name = "reviewing_department"
type = "category"
width = "u16"
render = true
values_of = "department"
listing = "per_viewer"
"#;
        let schema = parse_str(base).unwrap();
        assert_eq!(schema.attributes.len(), 2);
        // One compiled vocabulary, shared: two attributes, one code space.
        assert_eq!(schema.vocabularies.len(), 1);
        assert_eq!(schema.row_bits(), Some(32));

        let listing = base.replacen("listing = \"per_viewer\"\n", "listing = \"public\"\n", 1);
        // The *second* occurrence is the sharer's; replacing the first makes the referent public.
        assert!(err(&listing).contains("must agree"), "{}", err(&listing));

        let width = base.replace(
            "width = \"u16\"\nrender = true\nvalues_of",
            "width = \"u8\"\nrender = true\nvalues_of",
        );
        assert!(err(&width).contains("one code space"), "{}", err(&width));
    }

    #[test]
    fn a_column_may_not_shadow_a_fixed_or_reserved_name() {
        for name in ["tessera_id", "residual", "x", "access"] {
            let text = SEVERITY.replace("\"severity\"", &format!("\"{name}\""));
            assert!(err(&text).contains("shadows"), "{name}");
        }
    }

    #[test]
    fn one_name_may_not_be_declared_twice() {
        let text = format!("{SEVERITY}{SEVERITY}");
        assert!(err(&text).contains("declared twice"));
    }

    /// The name addresses the column in `/v1/categories/{column}`, so it must survive a path
    /// segment. `.` is included because it is path-legal but is the one character that makes a
    /// segment ambiguous with the traversal forms a router normalises away.
    #[test]
    fn a_column_name_must_survive_a_path_segment() {
        for name in ["a/b", "a b", "a.b", "a%2Fb", "caté"] {
            let text = SEVERITY.replace("\"severity\"", &format!("\"{name}\""));
            assert!(err(&text).contains("its identifier on the wire"), "{name}");
        }
        // The ordinary shapes stay legal — the rule is a character set, not a style guide.
        for name in ["severity_2", "severity-2", "Severity2"] {
            let text = SEVERITY.replace("\"severity\"", &format!("\"{name}\""));
            assert!(parse_str(&text).is_ok(), "{name}");
        }
    }

    /// §4.4: the two spellings of one thing, and the binding that names nothing.
    #[test]
    fn the_value_set_spellings_are_mutually_exclusive_and_bindings_must_be_claimed() {
        let both = SEVERITY.replace(
            "  [attribute.values]",
            "values_key = \"sev\"\n  [attribute.values]",
        );
        assert!(
            err(&both).contains("spellings of one thing"),
            "{}",
            err(&both)
        );

        // A private directory, for `parse_str`'s reason — a fixed name in the shared temp dir is
        // the same collision one step less likely.
        let dir = tempfile::tempdir().expect("tempdir");
        let unclaimed: HashMap<String, PathBuf> =
            [("nobody".to_string(), dir.path().join("nothing.parquet"))].into();
        let path = dir.path().join("schema.toml");
        std::fs::write(&path, SEVERITY).unwrap();
        let message = format!(
            "{}",
            Schema::parse(&path, &unclaimed).expect_err("expected a refusal")
        );
        assert!(message.contains("no attribute declares"), "{message}");
    }

    /// A mistyped disclosure control must not read as an absent one. The same
    /// `deny_unknown_fields` is what makes the retired placement key refuse loudly rather than
    /// read as an empty placement (decision 0048: the surface is replaced, not aliased).
    #[test]
    fn an_unknown_key_is_refused_rather_than_ignored() {
        let text = SEVERITY.replace("listing =", "listnig =");
        let message = err(&text);
        assert!(
            message.contains("listnig") || message.contains("unknown"),
            "{message}"
        );
    }
}
