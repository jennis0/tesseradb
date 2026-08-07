//! `schema.toml`: the caller declares what per-item data is *for*, and the placement follows.
//!
//! Per-point-attributes §2 is the design. A caller says `used_for = ["render"]` and gets a
//! fixed-width column in `columns.arrow`; §10.3's other two cadences — `filter` (an entity-space
//! posting) and `inspect` (a cold sidecar) — are declarable in the same file and **refused at
//! parse**, each naming itself, per decision 0013. Nothing here derives a placement from the
//! shape of the data file: a column that costs 0.93 GiB per byte per row per 10⁹ items is
//! declared or it does not exist (§4.1).
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
//! - **`listing = "public"` with `vocabulary = "discovered"`** (§3.8). A discovered vocabulary's
//!   values are inferred from whatever is in the corpus, so publishing them discloses
//!   data-derived names on nobody's authority.
//! - **Disagreement with a `values_of` referent** on `listing`, `vocabulary` or `width` (§3.9),
//!   or the weaker setting governs both and the gated column's value set publishes through the
//!   published one.

use std::collections::{BTreeMap, HashMap, HashSet};
use std::path::{Path, PathBuf};

use serde::Deserialize;
use tessera_spatial::tiler::ScalarType;

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
    used_for: Vec<String>,
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
    #[serde(default)]
    multi: Option<bool>,
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
    /// The hot column's width. Every attribute reaching this struct has one, `render` being the
    /// only placement built (§1).
    pub ty: ScalarType,
    /// The vocabulary this column's values are drawn from, for a category; `None` for a plain
    /// numeric attribute. Names a key in [`Schema::vocabularies`].
    pub vocabulary: Option<String>,
}

/// A named value set: keys, their pinned codes, and per-value presentation.
#[derive(Debug, Clone)]
pub struct Vocabulary {
    pub name: String,
    /// `per_viewer` or `public` (§3.8). Recorded and published; **not yet enforced anywhere**,
    /// there being no `/v1/categories` to filter — see [`Schema::parse`]'s ⊘ note.
    pub listing: Listing,
    /// Value key → code. Codes are pinned by the author and never minted here: minting is for
    /// `vocabulary = "discovered"`, which is refused (⊘).
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
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Listing {
    PerViewer,
    Public,
}

impl Listing {
    pub fn as_str(self) -> &'static str {
        match self {
            Listing::PerViewer => "per_viewer",
            Listing::Public => "public",
        }
    }
}

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
    /// **⊘ Specified, not implemented**, each refused at parse rather than accepted and ignored:
    /// `filter` and `inspect` in `used_for` (§1 — there is no attribute dictionary or postings
    /// file, and no `inspect` sidecar); `multi = true` (§3.7); and `vocabulary = "discovered"`,
    /// which needs the mint-and-record path §3.4 specifies and ingest's auto-mint arm.
    ///
    /// **⊘ `listing` is recorded and not enforced.** It is required, parsed and written to the
    /// manifest, but no endpoint publishes a vocabulary yet, so `per_viewer` currently gates
    /// nothing. It is required now rather than later because §4.3 makes absence a build error for
    /// a disclosure control, and because a bundle built without one would have to be rebuilt to
    /// acquire it.
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
            let placement = Placement::parse(&decl.used_for, &decl.name)?;
            if decl.multi == Some(true) {
                return Err(schema_error(format!(
                    "attribute '{}': `multi = true` is specified and not built \
                     (per-point-attributes §3.7). Multi-valued attributes are postings-only — \
                     admissible under `filter` and `inspect`, never under `render`, a rendered \
                     mark having one colour",
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
            if !placement.render {
                return Err(schema_error(format!(
                    "attribute '{}': `used_for` must contain \"render\". `filter` and `inspect` \
                     are specified and not built (per-point-attributes §1), so an attribute \
                     declaring neither `render` nor an unbuilt placement would declare nothing",
                    decl.name
                )));
            }

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
                    vocabularies.entry(vocab_name.clone()).or_insert(vocab);
                    Attribute {
                        name: decl.name.clone(),
                        ty,
                        vocabulary: Some(vocab_name),
                    }
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
                             timestamp_us, utf8 and category",
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
                    if ty == ScalarType::Utf8 {
                        return Err(schema_error(format!(
                            "attribute '{}': `render` on `utf8` is refused \
                             (per-point-attributes §4.3 — a non-fixed-width type in the hot \
                             column). A per-row string is the vocabulary stored once per row; \
                             declare a category, whose row cost is its width",
                            decl.name
                        )));
                    }
                    Attribute {
                        name: decl.name.clone(),
                        ty,
                        vocabulary: None,
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

    /// **Bits** this schema adds to every row, and `None` if any column is variable-width.
    ///
    /// §2.3's residency figure, **totalled across attributes rather than reported per column** —
    /// several categories are what makes the cost bite, and a per-column table lets each one look
    /// affordable. Bits rather than bytes because a `bool` costs one: rounding it to a byte would
    /// erase the whole reason to declare one.
    pub fn row_bits(&self) -> Option<u64> {
        self.attributes.iter().map(|a| a.ty.row_bits()).sum()
    }

    /// Whether this schema declares any column at all — the empty case being every bundle built
    /// before `--schema` existed, which must stay buildable and byte-identical.
    pub fn is_empty(&self) -> bool {
        self.attributes.is_empty()
    }
}

/// Which of §10.3's three cadences an attribute declares.
struct Placement {
    render: bool,
}

impl Placement {
    fn parse(used_for: &[String], attribute: &str) -> Result<Placement> {
        let mut render = false;
        for use_ in used_for {
            match use_.as_str() {
                "render" => render = true,
                // Named individually, each stating what is absent rather than "unsupported":
                // decision 0013's rule is that present-tense about absent machinery reads as an
                // assurance, and so does a generic refusal that hides which half is missing.
                "filter" => {
                    return Err(schema_error(format!(
                        "attribute '{attribute}': `filter` is specified and not built \
                         (per-point-attributes §1 and §3.5). It needs the attribute dictionary \
                         and its postings file, kept separate from the auth dictionary so that a \
                         caller-supplied attribute descriptor cannot byte-equal a satisfied auth \
                         descriptor; neither exists"
                    )));
                }
                "inspect" => {
                    return Err(schema_error(format!(
                        "attribute '{attribute}': `inspect` is specified and not built \
                         (per-point-attributes §1). §8.3's vector sidecar and §10.3's \
                         per-interaction row are one slot, whose first occupant — the external-ID \
                         store — is explicitly transitional"
                    )));
                }
                other => {
                    return Err(schema_error(format!(
                        "attribute '{attribute}': unknown `used_for` entry '{other}'. The three \
                         are \"render\", \"filter\" and \"inspect\" — §10.3's three access \
                         cadences"
                    )));
                }
            }
        }
        Ok(Placement { render })
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
    match vocabulary_kind {
        "declared" => {}
        "discovered" => {
            return Err(schema_error(format!(
                "attribute '{}': `vocabulary = \"discovered\"` is specified and not built \
                 (per-point-attributes §3.4, §5). It needs codes minted at random from the \
                 unused space and recorded in the manifest — dense first-seen codes make a \
                 visible code a lower bound on vocabulary cardinality — and ingest's auto-mint \
                 arm. Declare the value set instead",
                decl.name
            )));
        }
        other => {
            return Err(schema_error(format!(
                "attribute '{}': `vocabulary = \"{other}\"` is neither \"declared\" nor \
                 \"discovered\"",
                decl.name
            )));
        }
    }

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
    // §3.8's one incoherent combination, and it is a rule rather than a warning. Unreachable
    // while `discovered` is refused above; stated here so lifting that refusal cannot quietly
    // lift this one too.
    if listing == Listing::Public && vocabulary_kind == "discovered" {
        return Err(schema_error(format!(
            "attribute '{}': `listing = \"public\"` requires `vocabulary = \"declared\"` \
             (per-point-attributes §3.8). A discovered vocabulary's values are inferred from \
             whatever is in the corpus, so publishing them discloses data-derived names on \
             nobody's authority",
            decl.name
        )));
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
    } else {
        return Err(schema_error(format!(
            "attribute '{}': `vocabulary = \"declared\"` needs its value set — one of an inline \
             `[attribute.values]` block, a `values_key` bound at build, or a `values_of` \
             reference (per-point-attributes §4.4)",
            decl.name
        )));
    };

    check_codes(&codes, &reserved, ty, &decl.name)?;

    Ok((
        Vocabulary {
            // A vocabulary declared inline is named for its attribute; one from a file keeps the
            // logical key, so two attributes binding the same key share one compiled vocabulary.
            name: decl.values_key.clone().unwrap_or_else(|| decl.name.clone()),
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
) -> Result<()> {
    if codes.is_empty() {
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
    if FIXED.contains(&name) {
        return Err(schema_error(format!(
            "attribute '{name}' shadows a fixed column of `columns.arrow` (contracts §2.6). The \
             reader refuses such a segment at load"
        )));
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
    /// its input with `str::replace` on `"used_for = [...]"` silently stopped substituting when
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
used_for = ["render"]
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

    /// Decision 0013: absent machinery names itself rather than refusing generically.
    #[test]
    fn the_unbuilt_placements_and_discovered_vocabularies_name_themselves() {
        let filter = SEVERITY.replace("[\"render\"]", "[\"render\", \"filter\"]");
        assert!(err(&filter).contains("§3.5"), "{}", err(&filter));

        let inspect = SEVERITY.replace("[\"render\"]", "[\"render\", \"inspect\"]");
        assert!(err(&inspect).contains("sidecar"), "{}", err(&inspect));

        let discovered = SEVERITY.replace("\"declared\"", "\"discovered\"");
        assert!(err(&discovered).contains("minted at random"));

        let multi = SEVERITY.replace(
            "used_for = [\"render\"]",
            "used_for = [\"render\"]\nmulti = true",
        );
        assert!(err(&multi).contains("§3.7"), "{}", err(&multi));
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

    /// §4.3: a non-fixed-width type in the hot column. The capability exists in the storage
    /// layer — `ScalarType::Utf8` is writable — and is refused here, at the declaration.
    #[test]
    fn render_on_utf8_is_refused_at_the_declaration() {
        let text = r#"
[[attribute]]
name = "title"
type = "utf8"
used_for = ["render"]
"#;
        assert!(err(text).contains("non-fixed-width"), "{}", err(text));
    }

    #[test]
    fn a_plain_scalar_needs_none_of_the_vocabulary_machinery() {
        let text = r#"
[[attribute]]
name = "ingested_at"
type = "i64"
used_for = ["render"]

[[attribute]]
name = "score"
type = "f32"
used_for = ["render"]
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
used_for = ["render"]
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
used_for = ["render"]
vocabulary = "declared"
listing = "per_viewer"
  [attribute.values]
  eng = 1
  finance = 2

[[attribute]]
name = "reviewing_department"
type = "category"
width = "u16"
used_for = ["render"]
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
            "width = \"u16\"\nused_for = [\"render\"]\nvalues_of",
            "width = \"u8\"\nused_for = [\"render\"]\nvalues_of",
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

    /// A mistyped disclosure control must not read as an absent one.
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
