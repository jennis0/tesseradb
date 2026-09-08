//! Attribute columns declared while the service runs: `PUT /control/attributes`
//! (`ingest.md` §1.3, §6.3; decision 0136).
//!
//! A declaration is the `[[attribute]]` block minus its acquisition keys. The executor resolves it
//! against the served schema and the live bindings, appends an `AttributeDeclare` record, and
//! publishes a generation whose manifest carries the column at the tail of the scalar order. From
//! the acknowledgement the column exists for resolution: a batch may carry it, `/v1/meta` lists
//! it, and every reader answers absence for an entity nothing has filled it for.
//!
//! **The column appends and never inserts.** A buffered row's scalars, a record blob's field tags
//! and a flush's writer schema are positional against `declared_scalars`, so a runtime column
//! takes the next position and keeps it: the served list is the build's columns followed by the
//! runtime ones in declaration order, and a fold writes that list into the next `MANIFEST.json`
//! unchanged. A row buffered under the shorter arity is padded with each new column's absence
//! at the commit window's close and at the flush ([`absent_scalar`]), so one flush writes one
//! schema.
//!
//! **`render` is not accepted here** (decision 0136's amendment, 2026-09-08). A rendered value
//! lives in the hot column of the row that carries it, and this route addresses entities rather
//! than rows, so a column declared here has nowhere to put one. The refusal is an interim: what a
//! rendered column arriving at a running service should mean has not been worked through, and no
//! invariant forbids it. [`resolve`] carries the reason at the site.
//!
//! **The rules are the build's, transcribed.** `tessera_build::config::compile_attributes` is
//! the other statement of what a declaration may say, over the config's own types; the engine
//! does not depend on the build crate, so the rules are restated here and a change to one is a
//! change to both. Decision 0091 is the reason they must agree: a declaration a build accepts and
//! ingest refuses, or the reverse, is a feature that works at one door and not the other.

use tessera_lifecycle::wal::{AttributeDeclaration, WalScalar};
use tessera_lifecycle::{AttributeRequest, ExecError};
use tessera_spatial::tiler::ScalarType;
use tessera_store::manifest::{DeclaredScalar, Manifest, ScopedScalar};
use tessera_store::vocabulary::Vocabularies;
use tessera_types::layer::LayerScope;

/// A declaration compiled to the manifest entry it becomes: one of the flat bundle-wide columns,
/// or one group's column family (`views.md` §5).
#[derive(Debug, Clone, PartialEq)]
pub(crate) enum CompiledAttribute {
    Entity(DeclaredScalar),
    Scoped(ScopedScalar),
}

impl CompiledAttribute {
    /// The vocabulary a category column draws on, and the width its rows store it at.
    pub(crate) fn category(&self) -> Option<(&str, ScalarType)> {
        match self {
            CompiledAttribute::Entity(d) => d.vocabulary.as_deref().map(|v| (v, d.arrow_type)),
            CompiledAttribute::Scoped(f) => f.vocabulary.as_deref().map(|v| (v, f.arrow_type)),
        }
    }

    /// The record the log carries for this column, at the width a category stores.
    pub(crate) fn declaration(&self, title: Option<String>) -> AttributeDeclaration {
        match self {
            CompiledAttribute::Entity(d) => AttributeDeclaration {
                name: d.name.clone(),
                title,
                ty: d.arrow_type.arrow_type_name().to_string(),
                vocabulary: d.vocabulary.clone(),
                analyser: d.analyser.clone(),
                index: d.index,
                render: d.render,
                scope: LayerScope::Entity,
            },
            CompiledAttribute::Scoped(f) => AttributeDeclaration {
                name: f.name.clone(),
                title,
                ty: f.arrow_type.arrow_type_name().to_string(),
                vocabulary: f.vocabulary.clone(),
                analyser: f.analyser.clone(),
                index: f.index,
                render: f.render,
                scope: LayerScope::Group(f.group.clone()),
            },
        }
    }
}

/// The columns declared at a running service and not yet written into a `MANIFEST.json` by a
/// fold, in declaration order: the segments manifest's `attributes` and `scoped_attributes`
/// lists, held live. Written only by the executor; read at every side-manifest publication.
#[derive(Debug, Clone, Default)]
pub(crate) struct RuntimeAttributes {
    entity: Vec<DeclaredScalar>,
    scoped: Vec<ScopedScalar>,
}

impl RuntimeAttributes {
    pub(crate) fn seed(entity: Vec<DeclaredScalar>, scoped: Vec<ScopedScalar>) -> Self {
        RuntimeAttributes { entity, scoped }
    }

    pub(crate) fn push(&mut self, compiled: CompiledAttribute) {
        match compiled {
            CompiledAttribute::Entity(d) => self.entity.push(d),
            CompiledAttribute::Scoped(f) => self.scoped.push(f),
        }
    }

    /// Complete current state, for a publication.
    pub(crate) fn snapshot(&self) -> (Vec<DeclaredScalar>, Vec<ScopedScalar>) {
        (self.entity.clone(), self.scoped.clone())
    }

    /// The entity-scoped names, which is the set of columns whose base artefacts do not exist:
    /// what the opener must not demand a base for and the fold must not read one from.
    pub(crate) fn entity_names(&self) -> Vec<String> {
        self.entity.iter().map(|d| d.name.clone()).collect()
    }

    /// Drop the columns a fold has just written into `MANIFEST.json`, named at its plan. A
    /// declaration made while the fold ran is not among them and stays.
    pub(crate) fn retire_folded(&mut self, entity: &[String], scoped: &[String]) {
        self.entity.retain(|d| !entity.contains(&d.name));
        self.scoped.retain(|f| !scoped.contains(&f.name));
    }
}

/// What resolving a request against the served schema decided.
pub(crate) enum Resolution {
    /// A column of this name already carries exactly this identity: nothing to append.
    Existing,
    /// A new column, with the width a vocabulary no column named before must narrow to.
    New {
        compiled: CompiledAttribute,
        narrow: Option<(String, ScalarType)>,
    },
}

/// The names the segment writer and the ingest batch reserve, and the request surface's own
/// (`tessera_build::config::RESERVED_COLUMN_NAMES` and `check_column_name`, transcribed).
const FIXED_COLUMNS: [&str; 2] = ["tessera_id", "residual"];
const INGEST_RESERVED: [&str; 5] = ["external_id", "x", "y", "access", "node_id"];
const REQUEST_RESERVED: [&str; 6] = [
    "all_of",
    "any_of",
    "none_of",
    "region",
    "member_of",
    "highlighted",
];

/// Resolve a request against the served schema: refuse it, recognise it as a column already
/// held, or compile the column it declares.
///
/// `is_layer` answers whether a registered layer holds the name: an ingest batch's columns are
/// declared scalars or layer names, so a column under a layer's name would make a batch mean two
/// things.
pub(crate) fn resolve(
    request: &AttributeRequest,
    manifest: &Manifest,
    vocabularies: &Vocabularies,
    is_layer: impl Fn(&str) -> bool,
) -> Result<Resolution, ExecError> {
    let refused = |detail: String| ExecError::AttributeRefused { detail };
    let name = request.name.as_str();
    check_name(name).map_err(refused)?;
    if is_layer(name) {
        return Err(refused(format!(
            "attribute '{name}': a registered layer holds that name, and an ingest batch's \
             columns are declared scalars or layer names (contracts §3.4), so a column under it \
             would make a batch mean two things"
        )));
    }

    // **`render` is not accepted on this route** (decision 0136's amendment, 2026-09-08). A
    // rendered value is served from the hot column of the row that carries it, and this route
    // declares a column against entities rather than rows, so a declaration made here has nowhere
    // to put one. What a rendered column arriving at a running service should mean has not been
    // worked through: where the value lands for an entity that already holds rows, what a view
    // drawn before the declaration shows, and how the fold closes the gap. The refusal is an
    // interim that keeps a half-working path out of a deployment. There is no invariant against a
    // rendered column declared at a running service, and nothing here settles the question.
    //
    // It covers the flag and not the column, so a build column declared `render` cannot be
    // restated through this route either: the request carries `render = true` and is refused
    // before the held-name comparison. Restating a column changes nothing, so a caller who does
    // it loses nothing by being told to stop.
    if request.render {
        return Err(refused(format!(
            "attribute '{name}': `render` is not accepted at a running service. A rendered \
             value is served from the hot column of the row that carries it, and this route \
             declares a column against entities rather than rows, so there is nowhere to put \
             one. What a rendered column declared at a running service should mean has not been \
             worked through, and this refusal is an interim rather than a rule about rendered \
             columns (decision 0136's amendment). Declare the column without `render`, or \
             declare it at a build"
        )));
    }

    let compiled = compile(request, manifest, vocabularies).map_err(refused)?;

    // A name the schema holds is a held part (`ingest.md` §1.1): identical is accepted with no
    // effect, different is a conflict.
    if let Some(held) = held_by_name(manifest, name) {
        return if held == compiled {
            Ok(Resolution::Existing)
        } else {
            Err(ExecError::AttributeConflict {
                detail: format!(
                    "attribute '{name}' is already declared with a different type, vocabulary, \
                     analyser, flags or scope. A column's width and placement are baked into every \
                     row (per-point-attributes §2.2), so a name cannot change identity; declare \
                     the new column under another name"
                ),
            })
        };
    }

    // A vocabulary named by no column is seeded at the widest width; the first column to name it
    // fixes the width, and a code already bound past it is refused here rather than truncated.
    let narrow = match compiled.category() {
        Some((vocabulary, width)) if width_named_by_columns(manifest, vocabulary).is_none() => {
            let minter = vocabularies.get(vocabulary).ok_or_else(|| {
                refused(format!(
                    "attribute '{name}': `vocabulary = \"{vocabulary}\"` names no vocabulary this \
                     deployment carries. Declared: {}",
                    declared_vocabularies(manifest)
                ))
            })?;
            let mut probe = minter.clone();
            if let Err(code) = probe.narrow_to(width) {
                return Err(refused(format!(
                    "attribute '{name}': vocabulary '{vocabulary}' already binds code {code}, \
                     which a {} column cannot hold; declare the column at a width that holds \
                     every bound code",
                    width.arrow_type_name()
                )));
            }
            Some((vocabulary.to_string(), width))
        }
        _ => None,
    };
    Ok(Resolution::New { compiled, narrow })
}

/// A replayed record's column, at the width it recorded. The door validated the declaration
/// before the record was written, so what is compiled here is the record's own fields; a record
/// naming a group the manifest does not carry compiles to a family the manifest merge drops.
pub(crate) fn compile_record(declaration: &AttributeDeclaration) -> Option<CompiledAttribute> {
    let arrow_type = ScalarType::parse(&declaration.ty)?;
    Some(match &declaration.scope {
        LayerScope::Entity => CompiledAttribute::Entity(DeclaredScalar {
            name: declaration.name.clone(),
            arrow_type,
            vocabulary: declaration.vocabulary.clone(),
            analyser: declaration.analyser.clone(),
            index: declaration.index,
            render: declaration.render,
        }),
        LayerScope::Group(group) => CompiledAttribute::Scoped(ScopedScalar {
            name: declaration.name.clone(),
            group: group.clone(),
            arrow_type,
            vocabulary: declaration.vocabulary.clone(),
            analyser: declaration.analyser.clone(),
            index: declaration.index,
            render: declaration.render,
            views: Vec::new(),
        }),
    })
}

/// The column the served schema holds under `name`, if any, in the shape a compiled request takes
/// so the two compare field by field. A family's `views` list is the flushes' doing and not the
/// declaration's, so it is cleared for the comparison.
pub(crate) fn held_by_name(manifest: &Manifest, name: &str) -> Option<CompiledAttribute> {
    if let Some(d) = manifest.declared_scalars.iter().find(|d| d.name == name) {
        return Some(CompiledAttribute::Entity(d.clone()));
    }
    manifest
        .groups
        .iter()
        .flat_map(|g| g.scoped_scalars.iter())
        .find(|f| f.name == name)
        .map(|f| {
            let mut family = f.clone();
            family.views.clear();
            CompiledAttribute::Scoped(family)
        })
}

/// The width the columns already naming `vocabulary` store it at, or `None` where none does.
/// Every column over one vocabulary stores one width (`Vocabularies::seed` refuses otherwise),
/// so the first found is the answer.
fn width_named_by_columns(manifest: &Manifest, vocabulary: &str) -> Option<ScalarType> {
    manifest
        .declared_scalars
        .iter()
        .find(|d| d.vocabulary.as_deref() == Some(vocabulary))
        .map(|d| d.arrow_type)
        .or_else(|| {
            manifest
                .groups
                .iter()
                .flat_map(|g| g.scoped_scalars.iter())
                .find(|f| f.vocabulary.as_deref() == Some(vocabulary))
                .map(|f| f.arrow_type)
        })
}

fn declared_vocabularies(manifest: &Manifest) -> String {
    let names: Vec<&str> = manifest
        .vocabularies
        .iter()
        .map(|v| v.name.as_str())
        .collect();
    if names.is_empty() {
        "none".to_string()
    } else {
        names.join(", ")
    }
}

fn check_name(name: &str) -> Result<(), String> {
    if name.is_empty() {
        return Err("an attribute with an empty name".to_string());
    }
    if !name
        .bytes()
        .all(|b| b.is_ascii_alphanumeric() || b == b'_' || b == b'-')
    {
        return Err(format!(
            "attribute '{name}': a column name is its identifier on the wire \
             (`/v1/categories/{{column}}`, contracts §3.2), so it is limited to ASCII letters, \
             digits, `_` and `-`"
        ));
    }
    if FIXED_COLUMNS.contains(&name) {
        return Err(format!(
            "attribute '{name}': that name is a fixed column of every segment (contracts §2.6)"
        ));
    }
    if INGEST_RESERVED.contains(&name) {
        return Err(format!(
            "attribute '{name}': that name is reserved on the ingest batch (contracts §3.4), so a \
             declared column could not be carried under it"
        ));
    }
    if REQUEST_RESERVED.contains(&name) {
        return Err(format!(
            "attribute '{name}': that name is a filter combinator, a filter leaf or a frame \
             column of the request surface (decision 0062; `highlight-and-hierarchy.md` §2), and \
             a filter expression names columns directly, so a column may not take it. Reserved: {}",
            REQUEST_RESERVED.join(", ")
        ));
    }
    Ok(())
}

/// The build's `compile_attributes`, over a request: the type, its vocabulary or analyser, the
/// flag combinations the schema refuses, and the scope.
fn compile(
    request: &AttributeRequest,
    manifest: &Manifest,
    vocabularies: &Vocabularies,
) -> Result<CompiledAttribute, String> {
    let name = request.name.as_str();
    let (arrow_type, vocabulary, analyser) = match request.ty.as_str() {
        "category" => {
            let vocabulary = request.vocabulary.as_deref().ok_or_else(|| {
                format!(
                    "attribute '{name}': `vocabulary` is required for a category and has no \
                     default (configuration.md §6)"
                )
            })?;
            if !manifest.vocabularies.iter().any(|v| v.name == vocabulary)
                || vocabularies.get(vocabulary).is_none()
            {
                return Err(format!(
                    "attribute '{name}': `vocabulary = \"{vocabulary}\"` names no vocabulary this \
                     deployment carries. Declared: {}. A missing vocabulary is refused rather than \
                     minted: an implicit one would take a width, a value set and a visibility \
                     nobody declared",
                    declared_vocabularies(manifest)
                ));
            }
            if let Some(analyser) = &request.analyser {
                return Err(format!(
                    "attribute '{name}' is a category, not `text`, so `analyser = \"{analyser}\"` \
                     has no meaning for it. Refused rather than ignored"
                ));
            }
            let width = category_width(request, manifest, vocabulary)?;
            (width, Some(vocabulary.to_string()), None)
        }
        "utf8" => {
            return Err(format!(
                "attribute '{name}': `utf8` is retired as a declared type. A short string matched \
                 whole is `keyword`; prose searched by word is `text` (records-and-search §4.3, \
                 §4.4)"
            ));
        }
        other => {
            let ty = ScalarType::parse(other).ok_or_else(|| {
                format!(
                    "attribute '{name}': unknown type '{other}'. Declarable types are bool, u8, \
                     u16, u32, u64, i8, i16, i32, i64, f32, f64, timestamp_us, keyword, text and \
                     category"
                )
            })?;
            // A category-width type naming a vocabulary is the record's own spelling of a
            // category (`AttributeDeclaration::ty`), accepted at the door as well so a caller
            // may say the width where the block says `category` and the vocabulary says the width.
            if let Some(vocabulary) = request.vocabulary.as_deref() {
                if !ty.is_category_width() {
                    return Err(format!(
                        "attribute '{name}' is type '{other}', not a category, so `vocabulary` \
                         has no meaning for it. Refused rather than ignored: a value set on a \
                         column that has none is a disclosure control its author believes is set"
                    ));
                }
                let explicit = AttributeRequest {
                    ty: "category".to_string(),
                    width: Some(other.to_string()),
                    ..request.clone()
                };
                return compile(&explicit, manifest, vocabularies).and_then(|compiled| {
                    match compiled.category() {
                        Some((_, width)) if width == ty => Ok(compiled),
                        _ => Err(format!(
                            "attribute '{name}': vocabulary '{vocabulary}' is stored at another \
                             width by the columns that already name it"
                        )),
                    }
                });
            }
            if request.width.is_some() {
                return Err(format!(
                    "attribute '{name}' is type '{other}', not a category, so `width` has no \
                     meaning for it; the type is the width"
                ));
            }
            let analyser = match (ty, request.analyser.as_deref()) {
                (ScalarType::Text, analyser) => {
                    let analyser = analyser.unwrap_or(tessera_analyse::UNICODE);
                    let resolved = tessera_analyse::analyser(analyser).ok_or_else(|| {
                        format!(
                            "attribute '{name}': '{analyser}' is not an analyser this binary \
                             carries. Available: {}",
                            tessera_analyse::ANALYSER_NAMES.join(", ")
                        )
                    })?;
                    Some(resolved.identity())
                }
                (_, Some(analyser)) => {
                    return Err(format!(
                        "attribute '{name}' is type '{other}', not `text`, so `analyser = \
                         \"{analyser}\"` has no meaning for it. Refused rather than ignored"
                    ));
                }
                (_, None) => None,
            };
            // The two `render` refusals below state the build's rules over a request that
            // `resolve` has already refused for carrying `render` at all, so neither is reached
            // from this door. They are kept because this function is the engine's transcription
            // of `tessera_build::config::compile_attributes` (decision 0091): the build accepts
            // `render` and refuses these two types, and a transcription missing them would read
            // as a build rule that does not exist.
            if ty == ScalarType::Text && request.render {
                return Err(format!(
                    "attribute '{name}': `render` on `text` is refused; the hot column is a \
                     fixed-width slot in every row and prose is not one (records-and-search §3, \
                     §4.4). `index = true` gives it a token index"
                ));
            }
            if ty == ScalarType::Keyword && request.render {
                return Err(format!(
                    "attribute '{name}': `render` on `keyword` is refused (configuration.md §6); \
                     a keyword's value is not a fixed-width slot. Declare a category, or \
                     `index = true`"
                ));
            }
            (ty, None, analyser)
        }
    };

    match &request.scope {
        LayerScope::Entity => Ok(CompiledAttribute::Entity(DeclaredScalar {
            name: name.to_string(),
            arrow_type,
            vocabulary,
            analyser,
            index: request.index,
            render: request.render,
        })),
        LayerScope::Group(group) => {
            let descriptor = manifest
                .groups
                .iter()
                .find(|g| &g.name == group)
                .ok_or_else(|| {
                    format!(
                        "attribute '{name}': `scope` names view group '{group}', which this \
                         deployment does not declare"
                    )
                })?;
            if let Some(owner) = &descriptor.members_of {
                return Err(format!(
                    "attribute '{name}': `scope` names '{group}', which declares `members` of \
                     '{owner}'; a family belongs to the group that owns the keys, so scope it to \
                     '{owner}' (views §3.3)"
                ));
            }
            if arrow_type == ScalarType::Text && !request.index {
                return Err(format!(
                    "attribute '{name}': a group-scoped `text` column requires `index = true`; \
                     the record blob is bundle-wide and a family has no slot in it, so the token \
                     index is its only home (configuration.md §6)"
                ));
            }
            Ok(CompiledAttribute::Scoped(ScopedScalar {
                name: name.to_string(),
                group: group.clone(),
                arrow_type,
                vocabulary,
                analyser,
                index: request.index,
                render: request.render,
                views: Vec::new(),
            }))
        }
    }
}

/// A category's width: the width the columns already naming its vocabulary store, which a
/// supplied `width` must agree with, or the supplied `width` where no column names it yet.
fn category_width(
    request: &AttributeRequest,
    manifest: &Manifest,
    vocabulary: &str,
) -> Result<ScalarType, String> {
    let name = request.name.as_str();
    let supplied = request
        .width
        .as_deref()
        .map(|w| {
            ScalarType::parse(w)
                .filter(|t| t.is_category_width())
                .ok_or_else(|| {
                    format!(
                        "attribute '{name}': `width = \"{w}\"` is not a code width; a vocabulary's \
                         width is `u8`, `u16` or `u32` (per-point-attributes §3.6)"
                    )
                })
        })
        .transpose()?;
    match (width_named_by_columns(manifest, vocabulary), supplied) {
        (Some(held), Some(width)) if held != width => Err(format!(
            "attribute '{name}': vocabulary '{vocabulary}' is stored at {} by the columns that \
             already name it, and a vocabulary is one code space whichever columns draw on it \
             (per-point-attributes §3.9); omit `width` or say {}",
            held.arrow_type_name(),
            held.arrow_type_name()
        )),
        (Some(held), _) => Ok(held),
        (None, Some(width)) => Ok(width),
        (None, None) => Err(format!(
            "attribute '{name}': no column names vocabulary '{vocabulary}' yet, so its code width \
             is not recorded; say `width` (`u8`, `u16` or `u32`), which fixes it for every column \
             that names the vocabulary after this one"
        )),
    }
}

/// The value a row carries for a declared column it was buffered without (`ingest.md` §6.3,
/// §7.1): a category's absence is its reserved code 0 at the column's width, in band; every other
/// family's is `Null`, which lands in the column's presence bitmap (decision 0064). The same
/// split the ingest boundary makes for a null cell.
pub(crate) fn absent_scalar(declared: &DeclaredScalar) -> WalScalar {
    if declared.vocabulary.is_none() {
        return WalScalar::Null;
    }
    match declared.arrow_type {
        ScalarType::U8 => WalScalar::U8(0),
        ScalarType::U16 => WalScalar::U16(0),
        _ => WalScalar::U32(0),
    }
}

/// Pad a row buffered under an earlier, shorter schema to the current arity, one absence per
/// column declared since. **The rule for a declaration mid-ingest** (`ingest.md` §7.1): a new
/// column appends at the tail, so every position a shorter row does hold keeps its meaning, and
/// the positions it lacks are the columns that did not exist when it was admitted, which it holds
/// nothing for. A row longer than the schema is refused at admission and never reaches here.
pub(crate) fn pad_to_schema(scalars: &mut Vec<WalScalar>, declared: &[DeclaredScalar]) {
    if scalars.len() >= declared.len() {
        return;
    }
    for d in &declared[scalars.len()..] {
        scalars.push(absent_scalar(d));
    }
}
