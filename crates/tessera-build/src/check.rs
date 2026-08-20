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

use crate::config::{ArtifactSource, Config, Extent, Fields, ENTITY_ID};
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

#[derive(Debug, Clone, Default)]
pub struct CheckReport {
    pub sources: Vec<SourceChecked>,
    pub findings: Vec<Finding>,
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
    for view in &config.views {
        check_view(view, &mut report);
    }
    check_layers(config, &mut report);
    report
}

/// Every attribute source, and the declared columns each one carries.
///
/// **One group per file, exactly as the build reads them.** An attribute names its own `source` or
/// takes `[defaults]`'s, so the columns a file must carry are the columns of the attributes that
/// named it — and a column reported missing is reported against the file that was supposed to hold
/// it rather than against a single corpus that no longer exists.
fn check_attribute_sources(config: &Config, report: &mut CheckReport) {
    // A column with no file to read it from — legal to declare, and nothing a build could do
    // (`configuration.md` §2). Reported here rather than refused, exactly as the build refuses it
    // only when it comes to read.
    let mut carried: Vec<usize> = config
        .attribute_sources
        .iter()
        .flat_map(|s| s.attributes.iter().copied())
        .collect();
    carried.sort_unstable();
    for (index, attribute) in config.schema.attributes.iter().enumerate() {
        if carried.binary_search(&index).is_err() {
            report.note(
                format!("attribute '{}'", attribute.name),
                "names no `source` and `[defaults]` declares none, so there is no file for the \
                 attribute pass to read this column from"
                    .to_string(),
            );
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
            }
        }
    }
}

fn check_view(view: &crate::config::View, report: &mut CheckReport) {
    let object = format!("view '{}'", view.name);
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
            Some(ArtifactSource::File { path, fields }) => {
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
