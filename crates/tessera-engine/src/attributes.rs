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
//! `render` is not supported on this route yet.

use tessera_lifecycle::wal::{AttributeDeclaration, WalScalar};
use tessera_lifecycle::{AttributeRequest, ExecError};
use tessera_spatial::tiler::ScalarType;
use tessera_store::declaration::{check_attribute, AttributeSpec};
use tessera_store::manifest::{DeclaredScalar, Manifest, ScopedScalar};
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
    New(CompiledAttribute),
}

/// Resolve a request against the served schema: refuse it, recognise it as a column already
/// held, or compile the column it declares.
///
/// `is_layer` answers whether a registered layer holds the name: an ingest batch's columns are
/// declared scalars or layer names, so a column under a layer's name would make a batch mean two
/// things.
pub(crate) fn resolve(
    request: &AttributeRequest,
    manifest: &Manifest,
    is_layer: impl Fn(&str) -> bool,
) -> Result<Resolution, ExecError> {
    let refused = |detail: String| ExecError::AttributeRefused { detail };
    let name = request.name.as_str();
    // An ingest batch's columns are attributes or layer names, so one name cannot be both.
    if is_layer(name) {
        return Err(refused(format!(
            "attribute '{name}': a layer already has that name"
        )));
    }
    // Not built: a rendered value lives in a row's hot column, and a column declared here has no
    // rows written for it yet.
    if request.render {
        return Err(refused(format!(
            "attribute '{name}': `render` is not supported for a column declared at a running \
             service; declare it without `render`, or at a build"
        )));
    }

    let compiled = compile(request, manifest).map_err(refused)?;

    // Declaring a held column again is accepted if nothing differs. Type and placement are
    // written into every row, so a difference is a conflict.
    if let Some(held) = held_by_name(manifest, name) {
        return if held == compiled {
            Ok(Resolution::Existing)
        } else {
            Err(ExecError::AttributeConflict {
                detail: format!(
                    "attribute '{name}' is already declared with a different type, vocabulary, \
                     analyser, flags or scope; declare the new column under another name"
                ),
            })
        };
    }

    Ok(Resolution::New(compiled))
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

fn compile(request: &AttributeRequest, manifest: &Manifest) -> Result<CompiledAttribute, String> {
    let name = request.name.as_str();
    let group = match &request.scope {
        LayerScope::Entity => None,
        LayerScope::Group(group) => Some(group),
    };
    let column = check_attribute(
        &AttributeSpec {
            name,
            ty: &request.ty,
            vocabulary: request.vocabulary.as_deref(),
            analyser: request.analyser.as_deref(),
            index: request.index,
            render: request.render,
            group_scoped: group.is_some(),
        },
        |vocabulary| {
            manifest
                .vocabularies
                .iter()
                .find(|v| v.name == vocabulary)
                .map(|v| v.width)
        },
    )?;
    let Some(group) = group else {
        return Ok(CompiledAttribute::Entity(DeclaredScalar {
            name: name.to_string(),
            arrow_type: column.ty,
            vocabulary: column.vocabulary,
            analyser: column.analyser,
            index: request.index,
            render: request.render,
        }));
    };
    let descriptor = manifest
        .groups
        .iter()
        .find(|g| &g.name == group)
        .ok_or_else(|| format!("attribute '{name}': no view group named '{group}'"))?;
    if let Some(owner) = &descriptor.members_of {
        return Err(format!(
            "attribute '{name}': group '{group}' takes its members from '{owner}'; scope the \
             column to '{owner}'"
        ));
    }
    Ok(CompiledAttribute::Scoped(ScopedScalar {
        name: name.to_string(),
        group: group.clone(),
        arrow_type: column.ty,
        vocabulary: column.vocabulary,
        analyser: column.analyser,
        index: request.index,
        render: request.render,
        views: Vec::new(),
    }))
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
