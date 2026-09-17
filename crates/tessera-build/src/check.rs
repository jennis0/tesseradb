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

use std::path::Path;

use arrow::datatypes::{DataType, Schema as ArrowSchema};
use parquet::arrow::arrow_reader::ParquetRecordBatchReaderBuilder;
use tessera_spatial::tiler::ScalarType;

use crate::config::{
    ArtifactSource, Config, Extent, Fields, PointVisibility, Roster, ViewGroup, ENTITY_ID,
};
use crate::input::{column_carries, TERM_ID};

/// One thing wrong, named the way the reader that would have refused it names it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Finding {
    /// The declaration that owns it — `source 'points'`, `view 's0'`, `layer 'clusters/a'`.
    pub object: String,
    pub detail: String,
}

/// One source this check opened, for the summary line. **The count of what was looked at is part
/// of the answer**: a check that silently examined nothing passes just as loudly as one that
/// examined everything.
#[derive(Debug, Clone)]
pub struct SourceChecked {
    pub object: String,
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

impl FramePreview {
    /// One line for the view, and one for the snap where there is one.
    pub fn print(&self) {
        match &self.snapped {
            None => eprintln!(
                "  {:<20} {}, `extent = \"auto\"` — the frame is fitted to the data, so it is not \
                 known until the build reads the points",
                self.view, self.projection
            ),
            Some((asked, snap)) => {
                let f = snap.square.bounds();
                eprintln!(
                    "  {:<20} {}, asked for lon [{}, {}], lat [{}, {}]",
                    self.view,
                    self.projection,
                    asked.lon_min,
                    asked.lon_max,
                    asked.lat_min,
                    asked.lat_max
                );
                eprintln!(
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
                );
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

    fn note(&mut self, object: impl Into<String>, detail: impl Into<String>) {
        self.findings.push(Finding {
            object: object.into(),
            detail: detail.into(),
        });
    }

    fn warn(&mut self, object: impl Into<String>, detail: impl Into<String>) {
        self.warnings.push(Finding {
            object: object.into(),
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
    object: &str,
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
    object: &str,
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
fn open(report: &mut CheckReport, object: &str, path: &Path) -> Option<ArrowSchema> {
    report.sources.push(SourceChecked {
        object: object.to_string(),
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
    check_attribute_sources(config, &mut report);
    check_scoped_attribute_sources(config, &mut report);
    for view in &config.views {
        check_view(view, &mut report);
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
fn check_attribute_sources(config: &Config, report: &mut CheckReport) {
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
                object: format!("attribute '{}'", attribute.name),
                path: None,
            });
        }
    }
    for group in &config.attribute_sources {
        let object = format!("source '{}'", group.name);
        let Some(schema) = open(report, &object, &group.path) else {
            continue;
        };
        require(report, &object, &schema, &group.fields, ENTITY_ID);
        // **Every declared attribute against the field that must carry it.** Presence and family,
        // not fit: a `u8` column whose data carries 300 is a per-row refusal no schema can
        // anticipate.
        for &index in &group.attributes {
            let attribute = &config.schema.attributes[index];
            let object = format!("attribute '{}'", attribute.name);
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
            if !column_carries(attribute, field.data_type()) {
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
fn unique_key_by_footer(report: &mut CheckReport, object: &str, path: &Path, column: &str) {
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
        let object = format!("attribute '{}'", attribute.name);
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
            if column == attribute.column() && !column_carries(attribute, field.data_type()) {
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

fn check_view(view: &crate::config::View, report: &mut CheckReport) {
    let object = format!("view '{}'", view.name);
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
    require(report, &object, &schema, &view.fields, ENTITY_ID);

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
    let object = format!("view group '{}'", group.name);
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
                let object = format!("{object}, view '{}'", view.key);
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
        let object = format!("{object} `[view_group.views]`");
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
    object: &str,
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
    object: &str,
    point_visibility: &PointVisibility,
    schema: &ArrowSchema,
    report: &mut CheckReport,
) {
    let Some(field) = &point_visibility.field else {
        return;
    };
    match schema.column_with_name(field) {
        None => report.note(
            object,
            format!(
                "`point_visibility.field = \"{field}\"` names a column this view's source does not \
                 carry. Its columns are: {}",
                columns(schema)
            ),
        ),
        Some((_, found)) => {
            let ok = match found.data_type() {
                DataType::Utf8 => true,
                DataType::List(inner) | DataType::LargeList(inner) => {
                    matches!(inner.data_type(), DataType::Utf8)
                }
                _ => false,
            };
            if !ok {
                report.note(
                    object,
                    format!(
                        "the access column '{field}' holds {:?}. A point's access terms are \
                         strings — one, or a list of them",
                        found.data_type()
                    ),
                );
            }
        }
    }
}

/// Where each point's access terms come from: a column of the view's own source, or an exploded
/// relation of its own. Neither is required — `default` alone is the corpus with no permission
/// model — but a declared one that cannot be read puts every point in no principal's mask.
fn check_point_visibility(
    view: &crate::config::View,
    view_schema: Option<&ArrowSchema>,
    report: &mut CheckReport,
) {
    let object = format!("view '{}' `point_visibility`", view.name);
    if let Some(field) = &view.point_visibility.field {
        if let Some(schema) = view_schema {
            match schema.column_with_name(field) {
                None => report.note(
                    &object,
                    format!(
                        "`field = \"{field}\"` names a column the view's source does not carry. \
                         Its columns are: {}",
                        columns(schema)
                    ),
                ),
                Some((_, found)) => {
                    let ok = match found.data_type() {
                        DataType::Utf8 => true,
                        DataType::List(inner) | DataType::LargeList(inner) => {
                            matches!(inner.data_type(), DataType::Utf8)
                        }
                        _ => false,
                    };
                    if !ok {
                        report.note(
                            &object,
                            format!(
                                "the access column '{field}' holds {:?}. A point's access terms \
                                 are a `list<string>`, or a plain `string` where a point carries \
                                 one term",
                                found.data_type()
                            ),
                        );
                    }
                }
            }
        }
    }
    if let Some(path) = &view.point_visibility.source {
        let Some(schema) = open(report, &object, path) else {
            return;
        };
        let fields = Fields::canonical(object.clone());
        require(report, &object, &schema, &fields, ENTITY_ID);
        require(report, &object, &schema, &fields, TERM_ID);
    }
}

fn check_layers(config: &Config, report: &mut CheckReport) {
    for sources in &config.layer_sources {
        let object = format!("layer '{}'", sources.name);
        match &sources.artifacts {
            None => report.sources.push(SourceChecked {
                object: object.clone(),
                path: None,
            }),
            // Inline rows are the canonical spelling and there is no file to locate them in.
            Some(ArtifactSource::Inline(rows)) => report.sources.push(SourceChecked {
                object: format!("{object} ({} inline artifact(s))", rows.len()),
                path: None,
            }),
            Some(ArtifactSource::File { path, fields, .. }) => {
                if let Some(schema) = open(report, &object, path) {
                    // `key` is the one field a build-published artifact cannot do without: it is
                    // the address that survives a rebuild and what an edge into the layer names.
                    require(report, &object, &schema, fields, "key");
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
            let object = format!("{object} `[layer.members]`");
            if let Some(schema) = open(report, &object, &members.path) {
                require(report, &object, &schema, &members.fields, "key");
                require(report, &object, &schema, &members.fields, "entity");
                require_named(report, &object, &schema, &members.fields, &["rank"]);
            }
        }
    }
}
