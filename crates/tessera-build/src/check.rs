//! `tessera check` — the declaration against the files it names, **schemas only**.
//!
//! ## What it is for
//!
//! A build is minutes and writes a bundle; the mistakes it catches are mostly seconds old and
//! mostly typos. This verb is the seconds-long half: it parses the declaration — every refusal
//! `crate::config` states fires here, against no data at all — then opens each source's Parquet
//! **footer** and asks whether the columns the declaration named are there and can carry what it
//! said they carry. No row is decoded, so the cost is one seek per file and the verb is what a CI
//! job calls on every commit.
//!
//! ## Every finding, not the first
//!
//! A build refuses at the first thing wrong, which is right for a build: everything after it is
//! work nobody wants. A check exists to be run and fixed in one pass, so it collects. That is the
//! one behavioural difference from the build's own readers; the *rules* are theirs, and where a
//! rule lives in a reader this module states which reader it mirrors.
//!
//! ## What it cannot answer
//!
//! Everything that needs a row, which is worth naming because a green check is not a green build:
//! whether a closed vocabulary's keys cover the values in the data, whether a member id resolves
//! to an entity this build would assign, whether two artifact rows share a key, whether the
//! hierarchy's edges contain each other, and — the one that shipped a degenerate map — where the
//! data actually sits inside its view's extent. The last is the build's clamp report
//! (`crate::config::Frame`), and it needs the coordinate column read end to end.

use std::path::{Path, PathBuf};

use arrow::datatypes::{DataType, Schema as ArrowSchema};
use parquet::arrow::arrow_reader::ParquetRecordBatchReaderBuilder;
use tessera_spatial::tiler::ScalarType;
use tessera_store::scalar_column;

use crate::config::{
    ArtifactSource, Config, Extent, Fields, PointVisibility, Roster, ViewGroup, ENTITY_ID,
};
use crate::ids::Addressing;
use crate::input::TERM_ID;

/// The declaration a finding or a source is about, in its parts.
///
/// **Split rather than a sentence**, because the readers are two: an operator reads
/// `view group 'quarters' view 'q1'`, and a client that has to raise an error against the block
/// the author wrote reads the block and its name without parsing anything.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Object {
    /// The block kind as the declaration spells it — `source`, `attribute`, `view`,
    /// `view group`, `layer`.
    pub block: &'static str,
    /// The name the block was declared under.
    pub name: String,
    /// The part of it at fault, where a block has several: a group's view key, its roster table, a
    /// view's `point_visibility`, a layer's `[layer.members]`.
    pub part: Option<String>,
}

impl Object {
    pub fn new(block: &'static str, name: impl Into<String>) -> Object {
        Object {
            block,
            name: name.into(),
            part: None,
        }
    }

    /// The same block, narrowed to one part of it.
    pub fn part(&self, part: impl Into<String>) -> Object {
        Object {
            block: self.block,
            name: self.name.clone(),
            part: Some(part.into()),
        }
    }
}

impl std::fmt::Display for Object {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let mut rendered = format!("{} '{}'", self.block, self.name);
        if let Some(part) = &self.part {
            rendered.push(' ');
            rendered.push_str(part);
        }
        f.pad(&rendered)
    }
}

/// One thing wrong, named the way the reader that would have refused it names it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Finding {
    pub object: Object,
    pub detail: String,
}

/// One source this check opened, for the summary line. **The count of what was looked at is part
/// of the answer**: a check that silently examined nothing passes just as loudly as one that
/// examined everything.
#[derive(Debug, Clone)]
pub struct SourceChecked {
    pub object: Object,
    /// The path as the declaration resolved it, or `None` for an object declared with no source —
    /// which is legal and is the normal state for a deployment that writes through the service
    /// (`configuration.md` §2).
    pub path: Option<String>,
}

/// A projected view's frame, answered from the declaration alone (`projections.md` §4.2, §8).
///
/// **The one thing about a frame a check *can* settle.** A stated longitude/latitude box projects
/// and snaps with no data at all, so the square a corpus will be quantised against — and the
/// resolution the snap costs — are readable in seconds rather than after a build. Under `auto` the
/// frame is a function of the data, and this says so instead of guessing.
#[derive(Debug, Clone)]
pub struct FramePreview {
    pub view: String,
    pub projection: &'static str,
    /// The box declared, and the square it snaps to. `None` under `auto`.
    pub snapped: Option<(crate::config::LonLatBox, tessera_spatial::frame::Snap)>,
}

/// One line for the view, and one for the snap where there is one.
impl std::fmt::Display for FramePreview {
    fn fmt(&self, f_: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match &self.snapped {
            None => write!(
                f_,
                "  {:<20} {}, `extent = \"auto\"` — the frame is fitted to the data, so it is not \
                 known until the build reads the points",
                self.view, self.projection
            ),
            Some((asked, snap)) => {
                let f = snap.square.bounds();
                writeln!(
                    f_,
                    "  {:<20} {}, asked for lon [{}, {}], lat [{}, {}]",
                    self.view,
                    self.projection,
                    asked.lon_min,
                    asked.lon_max,
                    asked.lat_min,
                    asked.lat_max
                )?;
                write!(
                    f_,
                    "  {:<20} {} to the square at z{} ({}, {}) — x [{}, {}], y [{}, {}]",
                    "",
                    if snap.floored {
                        "FLOORED at the offset cap rather than fitted"
                    } else {
                        "snapped outward"
                    },
                    snap.square.z,
                    snap.square.x,
                    snap.square.y,
                    f.x_min,
                    f.x_max,
                    f.y_min,
                    f.y_max
                )
            }
        }
    }
}

#[derive(Debug, Clone, Default)]
pub struct CheckReport {
    pub sources: Vec<SourceChecked>,
    pub findings: Vec<Finding>,
    /// Per projected view, the frame its declaration implies — the half of the frame report that
    /// needs no data.
    pub frames: Vec<FramePreview>,
    /// Per shape layer, what its geometry is — computed from the geometry alone, before any
    /// build (`polygon-membership.md` §6.5); a layer that could not be sized says why.
    pub shapes: Vec<std::result::Result<crate::shapes::ShapeLayerReport, String>>,
    /// Things worth an operator's eye that refuse nothing and leave the check clean: an indexed
    /// keyword whose source footer says it is unique per row (`crate::unique_key`).
    pub warnings: Vec<Finding>,
}

impl CheckReport {
    pub fn is_clean(&self) -> bool {
        self.findings.is_empty()
    }

    fn note(&mut self, object: &Object, detail: impl Into<String>) {
        self.findings.push(Finding {
            object: object.clone(),
            detail: detail.into(),
        });
    }

    fn warn(&mut self, object: &Object, detail: impl Into<String>) {
        self.warnings.push(Finding {
            object: object.clone(),
            detail: detail.into(),
        });
    }
}

/// Read one Parquet file's schema — the footer alone, no row group decoded.
fn schema_of(path: &Path) -> std::result::Result<ArrowSchema, String> {
    let file = std::fs::File::open(path).map_err(|e| format!("{}: {e}", path.display()))?;
    let builder = ParquetRecordBatchReaderBuilder::try_new(file)
        .map_err(|e| format!("{}: not readable as Parquet: {e}", path.display()))?;
    Ok(builder.schema().as_ref().clone())
}

fn columns(schema: &ArrowSchema) -> String {
    let names: Vec<&str> = schema.fields().iter().map(|f| f.name().as_str()).collect();
    if names.is_empty() {
        "none".to_string()
    } else {
        names.join(", ")
    }
}

/// The type of the column `canonical` resolves to, or `None` where the file does not carry it.
fn column_type<'a>(
    schema: &'a ArrowSchema,
    fields: &Fields,
    canonical: &str,
) -> Option<&'a DataType> {
    schema
        .column_with_name(fields.of(canonical))
        .map(|(_, field)| field.data_type())
}

/// Require the column `canonical` resolves to, naming both halves — the field and the column it
/// was looked for under. This is `crate::input`'s `field_index` refusal, made collectable.
fn require(
    report: &mut CheckReport,
    object: &Object,
    schema: &ArrowSchema,
    fields: &Fields,
    canonical: &str,
) -> bool {
    if column_type(schema, fields, canonical).is_some() {
        return true;
    }
    report.note(
        object,
        format!(
            "field `{canonical}` is read from a column named '{}', which the source does not \
             carry. Its columns are: {}",
            fields.of(canonical),
            columns(schema)
        ),
    );
    false
}

/// Every field the `fields` map explicitly named must exist — the readers' half of
/// `configuration.md` §8's rule, and the one the parser cannot make.
fn require_named(
    report: &mut CheckReport,
    object: &Object,
    schema: &ArrowSchema,
    fields: &Fields,
    candidates: &[&str],
) {
    for canonical in candidates {
        if fields.names(canonical) {
            require(report, object, schema, fields, canonical);
        }
    }
}

/// Open one source, or record why it could not be opened. `None` means: reported, move on.
fn open(report: &mut CheckReport, object: &Object, path: &Path) -> Option<ArrowSchema> {
    report.sources.push(SourceChecked {
        object: object.clone(),
        path: Some(path.display().to_string()),
    });
    match schema_of(path) {
        Ok(schema) => Some(schema),
        Err(detail) => {
            report.note(object, detail);
            None
        }
    }
}

/// Check a parsed declaration against the schemas of the files it names.
///
/// The declaration has already been through every parse refusal by the time this is called —
/// `Config::parse` is what does that — so nothing here re-checks a rule answerable from the
/// document alone.
pub fn check(config: &Config) -> CheckReport {
    let mut report = CheckReport::default();
    let positional = positional_points(config);
    check_attribute_sources(config, &positional, &mut report);
    check_scoped_attribute_sources(config, &mut report);
    for view in &config.views {
        check_view(config, view, &mut report);
    }
    for group in &config.view_groups {
        check_view_group(config, group, &mut report);
    }
    check_layers(config, &mut report);
    // **The shape report is the one part of a check that reads rows**, deliberately: the
    // decomposition's size is what an operator sizing a world-scale boundary set needs, and it is
    // known from the geometry alone. A finding above means the file the shapes would be read from
    // may not open, so the rows are read only on a clean schema.
    if report.is_clean() {
        report.shapes = crate::shapes::check_reports(config);
    }
    report
}

/// Every attribute source, and the declared columns each one carries.
///
/// **One group per file, exactly as the build reads them.** An attribute names its own `source` or
/// takes `[defaults]`'s, so the columns a file must carry are the columns of the attributes that
/// named it — and a column reported missing is reported against the file that was supposed to hold
/// it rather than against a single corpus that no longer exists.
fn check_attribute_sources(config: &Config, positional: &[PathBuf], report: &mut CheckReport) {
    // A column with no file to read it from. Legal to declare (`configuration.md` §2), and the
    // normal state for a deployment that writes its values through the service. It is one of the
    // sources this check looked at and found nothing to open, beside a group that names no points
    // file, rather than a finding: there is no file, so there is no schema to disagree with.
    let mut carried: Vec<usize> = config
        .attribute_sources
        .iter()
        .flat_map(|s| s.attributes.iter().copied())
        .collect();
    carried.sort_unstable();
    // A group-scoped attribute is not in the schema at all — it is a column family, read from
    // the group's views' points files, which [`check_view_group`] checks column by column
    // (`views.md` §5).
    for (index, attribute) in config.schema.attributes.iter().enumerate() {
        if carried.binary_search(&index).is_err() {
            report.sources.push(SourceChecked {
                object: Object::new("attribute", &attribute.name),
                path: None,
            });
        }
    }
    for group in &config.attribute_sources {
        let object = Object::new("source", &group.name);
        let Some(schema) = open(report, &object, &group.path) else {
            continue;
        };
        // **A file whose rows are named by their position needs no identity column for its own
        // attributes.** The group is the view's own points file, and the build joins the columns
        // of that file by position (`ids::Addressing`).
        if !positional.contains(&group.path) {
            require(report, &object, &schema, &group.fields, ENTITY_ID);
        }
        // **Every declared attribute against the field that must carry it.** Presence and family,
        // not fit: a `u8` column whose data carries 300 is a per-row refusal no schema can
        // anticipate.
        for &index in &group.attributes {
            let attribute = &config.schema.attributes[index];
            let object = Object::new("attribute", &attribute.name);
            let Some((_, field)) = schema.column_with_name(attribute.column()) else {
                report.note(
                    &object,
                    format!(
                        "declared type '{}', read from a column named '{}', which source '{}' \
                         does not carry. Its columns are: {}",
                        attribute.ty.arrow_type_name(),
                        attribute.column(),
                        group.name,
                        columns(&schema)
                    ),
                );
                continue;
            };
            let category = attribute.vocabulary.is_some();
            if !scalar_column::carries(attribute.ty, category, field.data_type()) {
                report.note(
                    &object,
                    format!(
                        "declared '{}'{}, and the column '{}' holds {:?}. The width is baked into \
                         every row, so it is taken from the declaration and the data must match it",
                        attribute.ty.arrow_type_name(),
                        match &attribute.vocabulary {
                            Some(v) => format!(" over vocabulary '{v}', whose keys arrive as utf8"),
                            None => String::new(),
                        },
                        attribute.column(),
                        field.data_type()
                    ),
                );
                continue;
            }
            if attribute.ty == ScalarType::Keyword && attribute.index {
                unique_key_by_footer(report, &object, &group.path, attribute.column());
            }
        }
    }
}

/// An indexed keyword whose source footer records a distinct count within a few per cent of its
/// non-null values (`crate::unique_key`): the one thing about a unique key a check that reads no
/// row can see, and only where the writer recorded it. A warning, never a finding.
fn unique_key_by_footer(
    report: &mut CheckReport,
    object: &Object,
    path: &Path,
    column: &str,
) {
    match crate::unique_key::footer_distinct_count(path, column) {
        Ok(Some(count)) => {
            if let Some(warning) = count.warning() {
                report.warn(object, warning);
            }
        }
        Ok(None) => {}
        Err(detail) => report.warn(
            object,
            format!(
                "the footer's statistics could not be read, so nothing is said about whether the \
                 indexed key is unique: {detail}"
            ),
        ),
    }
}

/// Every group-scoped attribute that declares a **source of its own** (`views.md` §5), against
/// that file: the entity id, the value column, and the discriminator that says which view each
/// row's value is for.
///
/// **The discriminator is the one field this check adds**, and it is the reason such a source is
/// admissible at all: without it the file would be read as entity space and one arbitrary view's
/// values would be taken as every view's. Whether the keys in it are the roster's is a per-row
/// question the build answers when it reads them, not one a schema can.
fn check_scoped_attribute_sources(config: &Config, report: &mut CheckReport) {
    for scoped in &config.scoped_attributes {
        let Some(source) = &scoped.source else {
            continue;
        };
        let attribute = &scoped.attribute;
        let object = Object::new("attribute", &attribute.name);
        let Some(schema) = open(report, &object, &source.path) else {
            continue;
        };
        for (what, column) in [
            ("the entity id", source.entity_id.as_str()),
            ("the value", attribute.column()),
            ("the view discriminator", source.view_field.as_str()),
        ] {
            let Some((_, field)) = schema.column_with_name(column) else {
                report.note(
                    &object,
                    format!(
                        "is scoped to view group '{}' and reads {what} from a column named \
                         '{column}', which its `source` does not carry. Its columns are: {}",
                        scoped.group,
                        columns(&schema)
                    ),
                );
                continue;
            };
            let category = attribute.vocabulary.is_some();
            if column == attribute.column()
                && !scalar_column::carries(attribute.ty, category, field.data_type())
            {
                report.note(
                    &object,
                    format!(
                        "declared '{}'{}, and the column '{column}' holds {:?}. The width is \
                         taken from the declaration and the data must match it",
                        attribute.ty.arrow_type_name(),
                        match &attribute.vocabulary {
                            Some(v) => format!(" over vocabulary '{v}', whose keys arrive as utf8"),
                            None => String::new(),
                        },
                        field.data_type()
                    ),
                );
            }
        }
    }
}

/// The plain views whose points file carries no identity column, by path.
///
/// A row of one of those files is named by its position in it (`configuration.md` §8), and so is
/// that file's own attribute column, so [`check_attribute_sources`] does not require an identity
/// column of it either. Whether the positional route is admissible at all is
/// [`check_identity`]'s question, asked per view.
fn positional_points(config: &Config) -> Vec<PathBuf> {
    config
        .views
        .iter()
        .filter_map(|view| {
            let path = view.source.as_ref()?;
            let schema = schema_of(path).ok()?;
            column_type(&schema, &view.fields, ENTITY_ID)
                .is_none()
                .then(|| path.clone())
        })
        .collect()
}

/// A view's identity column, required only where something else in the declaration names a row by
/// it.
///
/// **The list is the build's own** (`ids::Addressing`), so a declaration this leaves clean is one
/// the build accepts. A points file carrying no identity column and nothing to name it for is a
/// warning saying how its rows are addressed, and the check stays clean.
fn check_identity(
    config: &Config,
    view: &crate::config::View,
    schema: &ArrowSchema,
    object: &Object,
    report: &mut CheckReport,
) {
    if column_type(schema, &view.fields, ENTITY_ID).is_some() {
        return;
    }
    let Some(points) = &view.source else {
        return;
    };
    let addressing = Addressing {
        points,
        several_views: config.views.len() > 1 || !config.view_groups.is_empty(),
        // A plain view's rows are its file's, whole. A selection belongs to a group's view, which
        // requires the identity column here whatever else the declaration says.
        selection: false,
        // `tessera check` takes no `--limit`. The build refuses one against this route.
        limit: false,
        visibility_source: view.point_visibility.source.as_deref(),
        attribute_sources: &config.attribute_sources,
        layers: &config.layers,
        layer_inputs: &config.layer_sources,
    };
    // A source that could not be read is reported where it is opened. Here it leaves the question
    // unanswered, which is a finding against the view like any other.
    let needed = match addressing.needs_identity() {
        Ok(needed) => needed,
        Err(error) => Some(error.to_string()),
    };
    match needed {
        None => report.warn(
            object,
            "no identity column: rows addressable by tessera_id only",
        ),
        Some(detail) => report.note(
            object,
            format!(
                "field `{ENTITY_ID}` is read from a column named '{}', which the source does not \
                 carry, so each row would be named by its position in it (contracts §2.4). \
                 {detail}",
                view.fields.of(ENTITY_ID)
            ),
        ),
    }
}

fn check_view(config: &Config, view: &crate::config::View, report: &mut CheckReport) {
    let object = Object::new("view", &view.name);
    // **The frame, before the file** — a projected view's square is a function of its declaration
    // alone, so it is answered here whether or not the source opens.
    if view.projection != tessera_spatial::Projection::None {
        report.frames.push(FramePreview {
            view: view.name.clone(),
            projection: view.projection.name(),
            snapped: match &view.extent {
                Extent::LonLat(asked) => {
                    Some((*asked, crate::config::snap_lon_lat(view.projection, asked)))
                }
                _ => None,
            },
        });
    }
    let Some(path) = &view.source else {
        report.sources.push(SourceChecked {
            object: object.clone(),
            path: None,
        });
        check_point_visibility(view, None, report);
        return;
    };
    let Some(schema) = open(report, &object, path) else {
        return;
    };
    check_identity(config, view, &schema, &object, report);

    // Which geometry shape this file offers, on `crate::input::geometry_kind`'s rule: a `fields`
    // map naming one is the caller deciding, and presence decides only where the map is silent.
    let names_morton = view.fields.names("morton") || view.fields.names("residual");
    let names_xy = view.fields.names("x") || view.fields.names("y");
    let has_morton = column_type(&schema, &view.fields, "morton").is_some();
    let morton = names_morton || (!names_xy && has_morton);
    if morton {
        require(report, &object, &schema, &view.fields, "morton");
        if view.fields.names("residual") {
            require(report, &object, &schema, &view.fields, "residual");
        }
        if matches!(view.extent, Extent::Auto { .. }) {
            report.note(
                &object,
                "`extent = \"auto\"` fits a box around coordinates, and this source stores Morton \
                 codes. Codes are exact only against the grid's own frame, so write it out: \
                 `extent = { min = 0.0, max = 65536.0 }`",
            );
        }
    } else {
        require(report, &object, &schema, &view.fields, "x");
        require(report, &object, &schema, &view.fields, "y");
    }
    check_point_visibility(view, Some(&schema), report);
}

/// A view group's files: each view's points under form A, the group's own under form B, and the
/// roster table where one is declared (`views.md` §3.1).
///
/// **Every view of a group is checked as a view**, because that is what it is below the
/// declaration: the same identity column, the same geometry pair, the same access column. What is
/// added is the discriminator — a form B source with no `view` column lands every row in a view
/// nobody named — and the group-scoped attribute columns, which live in the views' own files where
/// the attribute declares no source of its own (`views.md` §5).
fn check_view_group(config: &Config, group: &ViewGroup, report: &mut CheckReport) {
    let object = Object::new("view group", &group.name);
    if group.projection != tessera_spatial::Projection::None {
        report.frames.push(FramePreview {
            view: group.name.clone(),
            projection: group.projection.name(),
            snapped: match &group.extent {
                Extent::LonLat(asked) => {
                    Some((*asked, crate::config::snap_lon_lat(group.projection, asked)))
                }
                _ => None,
            },
        });
    }
    // The columns a scoped attribute reads out of this group's points files, where it declares no
    // source of its own — Appendix A's `sentiment`, read from each quarter's own file.
    // A `members` group's views are its owner's, and so are the files a column family scoped to
    // them is read from — this group's own points carry its geometry and nothing else
    // (`views.md` §3.3, §5).
    // A family declaring its **own** `source` is not among them: its values live in that file,
    // routed per view by the discriminator, and are checked against it in
    // [`check_scoped_attribute_sources`].
    let scoped: Vec<&str> = match group.members.is_some() {
        true => Vec::new(),
        false => config
            .scoped_attributes
            .iter()
            .filter(|scoped| scoped.group == group.name && scoped.source.is_none())
            .map(|scoped| scoped.attribute.column())
            .collect(),
    };

    match &group.roster {
        Roster::Inline(views) => {
            for view in views {
                let object = object.part(format!("view '{}'", view.key));
                let Some(path) = &view.source else {
                    report.sources.push(SourceChecked { object, path: None });
                    continue;
                };
                let Some(schema) = open(report, &object, path) else {
                    continue;
                };
                check_points(&object, &schema, &group.fields, false, report);
                check_group_labels(&object, &group.point_visibility, &schema, report);
                for column in &scoped {
                    if schema.column_with_name(column).is_none() {
                        report.note(
                            &object,
                            format!(
                                "the group-scoped attribute column '{column}' is read from each \
                                 view's own points file, and this one does not carry it. Its \
                                 columns are: {}. Declare the attribute's own `source` if the \
                                 values live elsewhere (views §5)",
                                columns(&schema)
                            ),
                        );
                    }
                }
            }
        }
        Roster::Table(_) | Roster::Discriminator => {
            let Some(path) = &group.source else {
                report.sources.push(SourceChecked { object, path: None });
                return;
            };
            let Some(schema) = open(report, &object, path) else {
                return;
            };
            check_points(&object, &schema, &group.fields, true, report);
            check_group_labels(&object, &group.point_visibility, &schema, report);
            for column in &scoped {
                if schema.column_with_name(column).is_none() {
                    report.note(
                        &object,
                        format!(
                            "the group-scoped attribute column '{column}' is read from this \
                             group's points, and the source does not carry it. Its columns are: \
                             {}. Declare the attribute's own `source` if the values live elsewhere \
                             (views §5)",
                            columns(&schema)
                        ),
                    );
                }
            }
        }
    }
    if let Roster::Table(table) = &group.roster {
        let object = object.part("`[view_group.views]`");
        let Some(schema) = open(report, &object, &table.source) else {
            return;
        };
        require(report, &object, &schema, &table.fields, "key");
        // `visibility` and the metadata names are located by the same map and required by the
        // group's own declaration: a metadata name declared is a column every view carries, and a
        // roster with the column missing serves the field absent for the life of every view.
        require_named(report, &object, &schema, &table.fields, &["visibility"]);
        for declared in &group.metadata {
            require(report, &object, &schema, &table.fields, &declared.name);
        }
    }
}

/// One points file's identity and geometry, and — where the group carries one — its discriminator.
fn check_points(
    object: &Object,
    schema: &ArrowSchema,
    fields: &Fields,
    discriminator: bool,
    report: &mut CheckReport,
) {
    require(report, object, schema, fields, ENTITY_ID);
    let names_morton = fields.names("morton") || fields.names("residual");
    let names_xy = fields.names("x") || fields.names("y");
    let has_morton = column_type(schema, fields, "morton").is_some();
    if names_morton || (!names_xy && has_morton) {
        require(report, object, schema, fields, "morton");
        if fields.names("residual") {
            require(report, object, schema, fields, "residual");
        }
    } else {
        require(report, object, schema, fields, "x");
        require(report, object, schema, fields, "y");
    }
    if discriminator {
        require(report, object, schema, fields, "view");
    }
}

/// A group's `point_visibility` against one of its views' files — [`check_point_visibility`]'s
/// rule, over a group's shared declaration rather than a view's own.
fn check_group_labels(
    object: &Object,
    point_visibility: &PointVisibility,
    schema: &ArrowSchema,
    report: &mut CheckReport,
) {
    if let Some(field) = &point_visibility.field {
        check_access_column(object, field, schema, report);
    }
}

/// A column named as where access labels are read from: present, and a string or a list of
/// strings. A view's `point_visibility.field` and a layer's `artifact_visibility.field` both name
/// one.
fn check_access_column(object: &Object, field: &str, schema: &ArrowSchema, report: &mut CheckReport) {
    if let Some(problem) = access_column_problem(field, schema) {
        report.note(object, problem);
    }
}

/// What is wrong with the column named as where access labels are read from, or `None`: it must be
/// present, and a string, a list of strings or a dictionary of strings. The check and the build's
/// reader of an artifact source both ask this.
pub(crate) fn access_column_problem(field: &str, schema: &ArrowSchema) -> Option<String> {
    let Some((_, found)) = schema.column_with_name(field) else {
        return Some(format!(
            "`{field}` is named as the access column, and this source does not carry it. Its \
             columns are: {}",
            columns(schema)
        ));
    };
    let ok = match found.data_type() {
        DataType::List(inner) | DataType::LargeList(inner) => crate::utf8::is_utf8(inner.data_type()),
        DataType::Dictionary(_, values) => crate::utf8::is_utf8(values),
        other => crate::utf8::is_utf8(other),
    };
    (!ok).then(|| {
        format!(
            "the access column '{field}' holds {:?}. Access labels are strings, one or a list of \
             them",
            found.data_type()
        )
    })
}

/// Where each point's access terms come from: a column of the view's own source, or an exploded
/// relation of its own. Neither is required — `default` alone is the corpus with no permission
/// model — but a declared one that cannot be read puts every point in no principal's mask.
fn check_point_visibility(
    view: &crate::config::View,
    view_schema: Option<&ArrowSchema>,
    report: &mut CheckReport,
) {
    let object = Object::new("view", &view.name).part("`point_visibility`");
    if let (Some(field), Some(schema)) = (&view.point_visibility.field, view_schema) {
        check_access_column(&object, field, schema, report);
    }
    if let Some(path) = &view.point_visibility.source {
        let Some(schema) = open(report, &object, path) else {
            return;
        };
        let fields = Fields::canonical(object.to_string());
        require(report, &object, &schema, &fields, ENTITY_ID);
        require(report, &object, &schema, &fields, TERM_ID);
    }
}

fn check_layers(config: &Config, report: &mut CheckReport) {
    for sources in &config.layer_sources {
        let object = Object::new("layer", &sources.name);
        match &sources.artifacts {
            None => report.sources.push(SourceChecked {
                object: object.clone(),
                path: None,
            }),
            // Inline rows are the canonical spelling and there is no file to locate them in, so
            // the rules the build's reader states over a row are answerable from the declaration:
            // `crate::shapes`'s `inline_shape`, which `crate::layers`'s `plan_inline` calls for
            // the same rows.
            Some(ArtifactSource::Inline(rows)) => {
                report.sources.push(SourceChecked {
                    object: object.part(format!("({} inline artifact(s))", rows.len())),
                    path: None,
                });
                if let Some(declaration) = config.layers.iter().find(|d| d.name == sources.name) {
                    let kind = crate::shapes::shape_declared(declaration);
                    for row in rows {
                        if let Err(detail) = crate::shapes::inline_shape(row, kind) {
                            report.note(&object, detail);
                        }
                    }
                }
            }
            Some(ArtifactSource::File { path, fields, .. }) => {
                if let Some(schema) = open(report, &object, path) {
                    // `key` is the one field a build-published artifact cannot do without: it is
                    // the address that survives a rebuild and what an edge into the layer names.
                    require(report, &object, &schema, fields, "key");
                    if let Some(field) = config
                        .layers
                        .iter()
                        .find(|d| d.name == sources.name)
                        .and_then(|d| d.artifact_visibility.field.as_deref())
                    {
                        check_access_column(&object, field, &schema, report);
                    }
                    require_named(
                        report,
                        &object,
                        &schema,
                        fields,
                        &[
                            "contents",
                            "parent",
                            "attached_layer",
                            "attached_key",
                            "members",
                            "excluding",
                            "min_x",
                            "min_y",
                            "max_x",
                            "max_y",
                            "cx",
                            "cy",
                            "r",
                            "a",
                            "b",
                            "angle",
                            "geometry",
                            "space",
                        ],
                    );
                }
            }
        }
        if let Some(members) = &sources.members {
            let object = object.part("`[layer.members]`");
            if let Some(schema) = open(report, &object, &members.path) {
                require(report, &object, &schema, &members.fields, "key");
                require(report, &object, &schema, &members.fields, "entity");
                require_named(report, &object, &schema, &members.fields, &["rank"]);
            }
        }
    }
}

/// The check as one page: what was read, what is wrong, what the declaration implies about frames,
/// view groups and shape layers, the disclosure decisions it makes, and the verdict.
///
/// Every caller renders through this — the binary, the Python extension module, and the SDK
/// through it — so the page is the same bytes whichever of them ran the check. The disclosure
/// table is written only on a clean check: a declaration with a finding in it has not been read
/// far enough for what it discloses to be worth reading.
pub fn page(config: &Config, report: &CheckReport) -> String {
    use std::fmt::Write;
    let mut out = String::new();
    for source in &report.sources {
        match &source.path {
            Some(path) => {
                let _ = writeln!(out, "  read schema  {:<34} {path}", source.object);
            }
            None => {
                let _ = writeln!(
                    out,
                    "  no source    {:<34} (declared and empty)",
                    source.object
                );
            }
        }
    }
    for finding in &report.findings {
        let _ = writeln!(out, "  FAILED       {}: {}", finding.object, finding.detail);
    }
    // A warning leaves the check clean and the exit status untouched: an indexed keyword the
    // source's footer says is unique per row is a cost to know about, not a mistake.
    for warning in &report.warnings {
        let _ = writeln!(out, "  WARNING      {}: {}", warning.object, warning.detail);
    }
    if !report.frames.is_empty() {
        let _ = writeln!(out, "projected views, from the declaration alone:");
        for frame in &report.frames {
            let _ = writeln!(out, "{frame}");
        }
    }
    if !config.view_groups.is_empty() {
        let _ = writeln!(out, "view groups, from the declaration alone:");
        for group in &config.view_groups {
            let keys = group.declared_keys();
            let roster = match (&group.members, keys.len()) {
                (Some(owner), _) => format!("the views of '{owner}'"),
                (None, 0) => group.form().to_string(),
                (None, n) => format!("{}, {n} view(s): {}", group.form(), keys.join(", ")),
            };
            let _ = writeln!(out, "  {:<20} {roster}", group.name);
            if !group.metadata.is_empty() {
                let _ = writeln!(
                    out,
                    "  {:<20} metadata: {}",
                    "",
                    group
                        .metadata
                        .iter()
                        .map(|m| format!("{} ({})", m.name, m.ty.arrow_type_name()))
                        .collect::<Vec<_>>()
                        .join(", ")
                );
            }
        }
        for (attribute, group) in &config.scopes.attributes {
            let _ = writeln!(
                out,
                "  {:<20} attribute '{attribute}'",
                format!("scope {group}")
            );
        }
        for (layer, group) in &config.scopes.layers {
            let _ = writeln!(out, "  {:<20} layer '{layer}'", format!("scope {group}"));
        }
    }
    if !report.shapes.is_empty() {
        let _ = writeln!(out, "shape layers, from the geometry alone:");
        for shape in &report.shapes {
            match shape {
                Ok(shape) => {
                    let _ = writeln!(out, "{shape}");
                }
                Err(why) => {
                    let _ = writeln!(out, "  not sized: {why}");
                }
            }
        }
    }
    if !report.is_clean() {
        let _ = writeln!(
            out,
            "check FAILED: {} finding(s) across {} source(s). Nothing was read but Parquet \
             schemas, so a clean check is not a clean build: it cannot see a value against a \
             closed vocabulary, a member id that resolves to nothing, or where the data sits \
             inside a view's extent",
            report.findings.len(),
            report.sources.len()
        );
        return out;
    }
    write_disclosure(&mut out, &crate::disclosure::Disclosure::of(config));
    let _ = writeln!(
        out,
        "\ncheck OK: {} source(s), {} view(s), {} view group(s) over {} declared view(s), {} \
         vocabulary(ies), {} attribute(s), {} layer(s), {} warning(s)",
        report.sources.len(),
        config.views.len(),
        config.view_groups.len(),
        config
            .view_groups
            .iter()
            .map(|g| g.declared_keys().len())
            .sum::<usize>(),
        config.schema.vocabularies.len(),
        // Every declared column, the group-scoped families included: they are held apart from the
        // schema because a family has no slot in the manifest's flat list, not because they are
        // fewer columns.
        config.schema.attributes.len() + config.scoped_attributes.len(),
        config.layers.len(),
        report.warnings.len()
    );
    out
}

/// The disclosure decisions a declaration makes, as a table an operator reads.
///
/// The same values `reports/disclosure.json` carries, and deliberately a second rendering of one
/// source rather than a second derivation: the file is for a diff between builds and this is for a
/// person deciding whether the declaration says what they meant.
fn write_disclosure(out: &mut String, disclosure: &crate::disclosure::Disclosure) {
    use std::fmt::Write;
    let _ = writeln!(out, "\nviews");
    for view in &disclosure.views {
        match &view.default {
            Some(default) => {
                let _ = writeln!(
                    out,
                    "  {:<26} labels from {}, default '{default}'",
                    view.name, view.labels_from
                );
            }
            None => {
                let _ = writeln!(
                    out,
                    "  {:<26} labels from {}, no default: an unlabelled point is refused",
                    view.name, view.labels_from
                );
            }
        }
    }
    if !disclosure.vocabularies.is_empty() {
        let _ = writeln!(out, "\nvocabularies");
        for vocabulary in &disclosure.vocabularies {
            let _ = writeln!(
                out,
                "  {:<26} {}, {}, {} declared value(s){}",
                vocabulary.name,
                vocabulary.visibility,
                vocabulary.value_set,
                vocabulary.declared_values,
                if vocabulary.reserved.is_empty() {
                    String::new()
                } else {
                    format!(", reserved {:?}", vocabulary.reserved)
                }
            );
        }
    }
    if !disclosure.attributes.is_empty() {
        let _ = writeln!(
            out,
            "\nattributes (in declaration order, which is the stored column order)"
        );
        for attribute in &disclosure.attributes {
            let _ = writeln!(
                out,
                "  {:<26} {}{}, {}, from column '{}'{}",
                attribute.name,
                attribute.ty,
                match &attribute.vocabulary {
                    Some(v) => format!(" over vocabulary '{v}'"),
                    None => String::new(),
                },
                attribute.placement,
                attribute.field,
                // A family is one column per view of the group, read from those views' own points
                // and stored under `attrs/<column>/<group>/<key>/`.
                match &attribute.scope {
                    Some(group) => format!(", one column per view of '{group}'"),
                    None => String::new(),
                }
            );
        }
    }
    if !disclosure.layers.is_empty() {
        let _ = writeln!(
            out,
            "\nlayers (in declaration order, which is registration order)"
        );
        for layer in &disclosure.layers {
            let _ = writeln!(out, "  {}", layer.name);
            if let Some(parent) = &layer.expanded_from {
                let _ = writeln!(out, "      written by `[layer.labels]` on '{parent}'");
            }
            let _ = writeln!(
                out,
                "      gate '{}' | artifacts {} | members {}",
                layer.visibility,
                match &layer.artifact_visibility.field {
                    Some(field) => format!(
                        "carry their own in '{field}', else '{}'",
                        layer.artifact_visibility.default
                    ),
                    None => format!("'{}'", layer.artifact_visibility.default),
                },
                match layer.require_member_visibility.as_str() {
                    Some(word) => word.to_string(),
                    None => layer.require_member_visibility.to_string(),
                }
            );
            if !layer.depends_on.is_empty() {
                let _ = writeln!(
                    out,
                    "      served only where {} is served",
                    layer.depends_on.join(", ")
                );
            }
            if !layer.content.computed.is_empty() {
                let _ = writeln!(out, "      computed {}", layer.content.computed.join(", "));
            }
            for supplied in &layer.content.supplied {
                let _ = writeln!(
                    out,
                    "      supplied {} '{}' requires {}",
                    supplied.ty, supplied.name, supplied.require_member_visibility
                );
            }
        }
    }
}
