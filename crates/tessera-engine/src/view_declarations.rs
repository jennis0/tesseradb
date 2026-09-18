//! View groups and plain views declared while the service runs: `PUT /control/view_groups/{name}`
//! and `PUT /control/views/{name}` (`ingest.md` §1.3 and §10 R9; decision 0136).
//!
//! A group declaration is the `[[view_group]]` block minus its roster and its source; a plain
//! view's is the `[[view]]` block minus its source. Both are durable at the acknowledgement and
//! carry an **empty row space** until their first flush, which is what a view created under a
//! group already gets (`views.md` §3.2).
//!
//! **This is what decision 0136's R9 overturns.** `views.md` §7 ruled that no plain view is added
//! after the build, on the argument that a view carries a frame and a gate and the design has one
//! place where those are reviewed. The rule decision 0134 states is the wider one: anything a
//! build can create, live ingest can create. The review of a frame and a gate is an operator's
//! question, and the control plane holds the operator credential.
//!
//! **The rules are the build's, transcribed**, on `crate::attributes`' argument (decision 0091).
//! What differs is the frame: a declaration here names the frame in **frame coordinates**
//! (`extent = { x = [a, b], y = [c, d] }`) and never `auto`, because there is no data to fit one
//! to and no source to survey. A projected view's frame is the projected box the build's
//! `lon`/`lat` spelling produces, so nothing is unsayable here — only spelled once rather than
//! twice.

use tessera_lifecycle::wal::{DeclaredFrame, PlainViewDeclaration, ViewGroupDeclaration};
use tessera_lifecycle::ExecError;
use tessera_store::manifest::{GroupDescriptor, Manifest, Quantisation, ViewDescriptor};
use tessera_types::view::{check_view_key, ViewMetadataType, DECLARED_INCARNATION};

/// The groups and plain views declared at a running service and not yet written into a
/// `MANIFEST.json` by a fold, in declaration order: the segments manifest's `groups` and
/// `plain_views` lists, held live. Written only by the executor; read at every side-manifest
/// publication.
///
/// **A group's roster is not held here.** The roster is `ViewRoster`'s and reaches the manifest
/// through `Manifest::with_roster`, so a descriptor here carries the group's own half and an
/// empty `views` list; holding a second copy would be a second place for the two to disagree
/// about which keys exist.
#[derive(Debug, Clone, Default)]
pub(crate) struct RuntimeViewDeclarations {
    groups: Vec<GroupDescriptor>,
    plain: Vec<ViewDescriptor>,
}

impl RuntimeViewDeclarations {
    pub(crate) fn seed(groups: Vec<GroupDescriptor>, plain: Vec<ViewDescriptor>) -> Self {
        RuntimeViewDeclarations { groups, plain }
    }

    pub(crate) fn push_group(&mut self, group: GroupDescriptor) {
        self.groups.push(group);
    }

    pub(crate) fn push_plain(&mut self, view: ViewDescriptor) {
        self.plain.push(view);
    }

    pub(crate) fn holds_group(&self, name: &str) -> bool {
        self.groups.iter().any(|g| g.name == name)
    }

    pub(crate) fn holds_plain(&self, id: &str) -> bool {
        self.plain.iter().any(|v| v.id == id)
    }

    /// Complete current state, for a publication.
    pub(crate) fn snapshot(&self) -> (Vec<GroupDescriptor>, Vec<ViewDescriptor>) {
        (self.groups.clone(), self.plain.clone())
    }

    /// The names a publication is about to write into `MANIFEST.json`, so the live lists can be
    /// emptied of exactly them once it has landed.
    pub(crate) fn names(&self) -> (Vec<String>, Vec<String>) {
        (
            self.groups.iter().map(|g| g.name.clone()).collect(),
            self.plain.iter().map(|v| v.id.clone()).collect(),
        )
    }

    /// Drop what a fold has just written into `MANIFEST.json`. A declaration made while the fold
    /// ran is not among them and stays.
    pub(crate) fn retire_folded(&mut self, groups: &[String], plain: &[String]) {
        self.groups.retain(|g| !groups.contains(&g.name));
        self.plain.retain(|v| !plain.contains(&v.id));
    }
}

/// What resolving a declaration against the served manifest decided.
pub(crate) enum Resolution<T> {
    /// An object of this name already carries exactly this identity: nothing to declare.
    Existing,
    /// A new one, compiled to the manifest entry it becomes.
    New(Box<T>),
}

/// Resolve a group declaration: refuse it, recognise the group already held, or compile it.
pub(crate) fn resolve_group(
    declaration: &ViewGroupDeclaration,
    manifest: &Manifest,
) -> Result<Resolution<GroupDescriptor>, ExecError> {
    let refused = |detail: String| ExecError::ViewRefused { detail };
    let name = declaration.name.as_str();
    check_view_key(name).map_err(refused)?;
    check_name_free(name, manifest, NameKind::Group).map_err(refused)?;
    let projection = parse_projection(name, &declaration.projection).map_err(refused)?;
    let quantisation = compile_frame(name, &declaration.frame).map_err(refused)?;

    // **A `members` group takes another group's keys** (`views.md` §3.3), so the owner must exist
    // and must own its own keys — chains are refused at the declaration, which is what lets every
    // reader take `members_of` as naming an owner.
    if let Some(owner) = &declaration.members {
        let Some(owner_descriptor) = manifest.groups.iter().find(|g| &g.name == owner) else {
            return Err(ExecError::ViewUnknown {
                detail: format!(
                    "view group '{name}' declares `members = \"{owner}\"`, which names no view \
                     group this deployment carries. A `members` group takes another group's keys \
                     (views §3.3), so the owner is declared first"
                ),
            });
        };
        if owner_descriptor.members_of.is_some() {
            return Err(refused(format!(
                "view group '{name}' declares `members = \"{owner}\"`, and '{owner}' takes its \
                 own views from another group. Chains are refused: name the group that owns the \
                 keys (views §3.3)"
            )));
        }
        if !declaration.metadata.is_empty() {
            return Err(refused(format!(
                "view group '{name}' declares `members` and `metadata`. Keys and metadata belong \
                 to the group that owns them, so a sharing group declares neither (views §3.3)"
            )));
        }
    }
    check_metadata(name, &declaration.metadata).map_err(refused)?;

    let compiled = GroupDescriptor {
        name: declaration.name.clone(),
        title: declaration.title.clone(),
        members_of: declaration.members.clone(),
        quantisation,
        projection,
        metadata: declaration.metadata.clone(),
        visibility: gate_of(declaration.visibility.clone()),
        point_default: declaration.point_default.clone(),
        // **Empty, and the roster fills it.** A group's keys are `ViewRoster`'s, applied by
        // `Manifest::with_roster` after this list is merged in, so a descriptor written with a
        // roster here would be a second copy of the keys.
        views: Vec::new(),
        scoped_scalars: Vec::new(),
    };
    if let Some(held) = manifest.groups.iter().find(|g| g.name == name) {
        return if same_group(held, &compiled) {
            Ok(Resolution::Existing)
        } else {
            Err(ExecError::ViewConflict {
                detail: format!(
                    "view group '{name}' is already declared with a different frame, projection, \
                     gate, point default, metadata or owner. A group's frame is immutable for its \
                     life (decision 0040) and its gate, once written, never changes (views §6), \
                     so a name cannot change identity; declare the new group under another name"
                ),
            })
        };
    }
    Ok(Resolution::New(Box::new(compiled)))
}

/// Resolve a plain view declaration, on [`resolve_group`]'s shape.
pub(crate) fn resolve_plain(
    declaration: &PlainViewDeclaration,
    manifest: &Manifest,
) -> Result<Resolution<ViewDescriptor>, ExecError> {
    let refused = |detail: String| ExecError::ViewRefused { detail };
    let name = declaration.name.as_str();
    check_view_key(name).map_err(refused)?;
    check_name_free(name, manifest, NameKind::View).map_err(refused)?;
    let projection = parse_projection(name, &declaration.projection).map_err(refused)?;
    let quantisation = compile_frame(name, &declaration.frame).map_err(refused)?;

    let compiled = ViewDescriptor {
        display_name: declaration
            .title
            .clone()
            .unwrap_or_else(|| declaration.name.clone()),
        id: declaration.name.clone(),
        // **The build's incarnation** (decision 0115). A plain view has no drop route, so its
        // name is created once and no predecessor's artifacts can be on disc under it; the
        // number exists to keep a *recreated* key from adopting them.
        incarnation: DECLARED_INCARNATION,
        quantisation,
        projection,
        visibility: gate_of(declaration.visibility.clone()),
        point_default: declaration.point_default.clone(),
    };
    if let Some(held) = manifest.views.iter().find(|v| v.id == name) {
        return if same_view(held, &compiled) {
            Ok(Resolution::Existing)
        } else {
            Err(ExecError::ViewConflict {
                detail: format!(
                    "view '{name}' is already declared with a different frame, projection, gate \
                     or point default. A view's frame is immutable for its life (decision 0040) \
                     and its gate, once written, never changes (views §6), so a name cannot \
                     change identity; declare the new view under another name"
                ),
            })
        };
    }
    Ok(Resolution::New(Box::new(compiled)))
}

/// Whether a name is being claimed for a group or for a view. Both are refused against both
/// lists: a group's name and a plain view's name are read at the same position of a request, so a
/// deployment carrying one of each under one name would answer two things to one word.
enum NameKind {
    Group,
    View,
}

fn check_name_free(name: &str, manifest: &Manifest, kind: NameKind) -> Result<(), String> {
    let clash = match kind {
        NameKind::Group => manifest.views.iter().any(|v| v.id == name),
        NameKind::View => manifest.groups.iter().any(|g| g.name == name),
    };
    if clash {
        let (mine, theirs) = match kind {
            NameKind::Group => ("view group", "view"),
            NameKind::View => ("view", "view group"),
        };
        return Err(format!(
            "{mine} '{name}': this deployment already declares a {theirs} of that name. A view id \
             and a group's name are read at the same position of a request (views §3.2), so one \
             word cannot name both"
        ));
    }
    Ok(())
}

fn parse_projection(name: &str, declared: &str) -> Result<tessera_spatial::Projection, String> {
    tessera_spatial::Projection::from_name(declared).ok_or_else(|| {
        format!(
            "'{name}': `projection = \"{declared}\"` is not a projection this build can place \
             points under (projections.md §5)"
        )
    })
}

/// The declared frame as the manifest stores it.
///
/// **Frame coordinates, and never `auto`.** A declaration at a running service has no source to
/// survey, so there is nothing for `auto` to fit; and the bounds are checked here because a
/// Morton code is a fraction of the frame, so a frame with no width places every point of the
/// view at one coordinate.
fn compile_frame(name: &str, frame: &DeclaredFrame) -> Result<Quantisation, String> {
    for (axis, min, max) in [
        ("x", frame.x_min, frame.x_max),
        ("y", frame.y_min, frame.y_max),
    ] {
        if !min.is_finite() || !max.is_finite() {
            return Err(format!(
                "'{name}': the `{axis}` extent is not a pair of finite numbers. A position is \
                 quantised as a fraction of the frame, so a bound that is not a number places \
                 every point of the view nowhere"
            ));
        }
        if min >= max {
            return Err(format!(
                "'{name}': the `{axis}` extent is [{min}, {max}], which has no width. A position \
                 is quantised as a fraction of the frame, so every point of the view would take \
                 one coordinate"
            ));
        }
    }
    Ok(Quantisation {
        x_min: frame.x_min,
        x_max: frame.x_max,
        y_min: frame.y_min,
        y_max: frame.y_max,
    })
}

/// The gate as the manifest stores it: a public view has none. The labels were checked against
/// the plugin before the declaration reached the executor.
fn gate_of(visibility: Option<Vec<String>>) -> Option<Vec<String>> {
    visibility.filter(|labels| labels.as_slice() != ["public"])
}

/// The names a roster record already uses, which a metadata name may not take
/// (`configuration.md` §1).
const ROSTER_NAMES: [&str; 3] = ["key", "source", "visibility"];

fn check_metadata(
    name: &str,
    metadata: &[tessera_types::view::GroupMetadataField],
) -> Result<(), String> {
    let mut seen = std::collections::BTreeSet::new();
    for field in metadata {
        if !seen.insert(field.name.as_str()) {
            return Err(format!(
                "view group '{name}': metadata '{}' is declared twice",
                field.name
            ));
        }
        if ROSTER_NAMES.contains(&field.name.as_str()) {
            return Err(format!(
                "view group '{name}': metadata '{}' takes a name the roster record already uses. \
                 Reserved: {}",
                field.name,
                ROSTER_NAMES.join(", ")
            ));
        }
        // ⊘ **A category-typed metadata name has no create that can satisfy it** at a running
        // service (`views.md` §3.2): a category's value is a key resolved to its vocabulary's
        // code, and the create route resolves none. Declaring one here would declare a group
        // whose views cannot be created, so it is refused at the declaration rather than at every
        // create.
        if field.ty == ViewMetadataType::Category {
            return Err(format!(
                "view group '{name}': metadata '{}' is a category, and a view of a group declared \
                 at a running service is created by a route that resolves no category value \
                 (views §3.2). Declare the name as a scalar, or declare the group at a build",
                field.name
            ));
        }
    }
    Ok(())
}

fn same_group(held: &GroupDescriptor, compiled: &GroupDescriptor) -> bool {
    held.title == compiled.title
        && held.members_of == compiled.members_of
        && same_frame(held.quantisation, compiled.quantisation)
        && held.projection == compiled.projection
        && held.metadata == compiled.metadata
        && held.visibility == compiled.visibility
        && held.point_default == compiled.point_default
}

fn same_view(held: &ViewDescriptor, compiled: &ViewDescriptor) -> bool {
    held.display_name == compiled.display_name
        && same_frame(held.quantisation, compiled.quantisation)
        && held.projection == compiled.projection
        && held.visibility == compiled.visibility
        && held.point_default == compiled.point_default
}

/// Two frames, compared by their four bounds. `Quantisation` carries `f64`s and derives no
/// equality, so this is the one definition; a `NaN` bound never reaches it, `compile_frame`
/// refusing one.
fn same_frame(held: Quantisation, compiled: Quantisation) -> bool {
    held.x_min == compiled.x_min
        && held.x_max == compiled.x_max
        && held.y_min == compiled.y_min
        && held.y_max == compiled.y_max
}
