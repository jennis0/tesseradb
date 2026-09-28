//! Which item each row of each file a build reads belongs to.
//!
//! A build is an ingest into an empty database, so its files are read under the rule an ingest is
//! ([`tessera_lifecycle::resolve`]), one file at a time in declaration order: each view's points,
//! each attribute file, each group-scoped attribute's own file, the access relation, and each
//! layer's memberships. A row names the item holding each non-null unique value it carries. A
//! points row naming none creates an item, and a row of any other file naming none is refused. A
//! row naming two items is refused, and so is a later row naming an item, or setting a unique
//! value, that an earlier row of its file names or sets. Every refused row is counted and
//! reported ([`RefusedRows`]), and `--strict` refuses the build at the first file with one.
//!
//! **Items are numbered in creation order**: the rows each view's points create, view by view, in
//! file order. The number is what every later pass joins a row on and what the entity-id
//! assignment breaks its last tie on. So the numbers are `0..n` with nothing to look up, a points
//! file's rows land in ascending numbers, and a pass that walks a file walks the items it touches
//! in order. [`Numbers`] holds every row's number for each file; a row with none is skipped by
//! every reader.
//!
//! The streaming build applies the rule by sorting each file's values and merging them against
//! the values earlier files gave items (`stream`), because a lookup per row is random access at
//! 10⁹ rows. The linear build asks [`tessera_lifecycle::resolve::resolve`] file by file over maps
//! (`linear`). This module's tests hold the two numberings equal file by file, and
//! `tests/identity_rule.rs` the two builds byte-identical, which is what checks the sort-merge
//! against the rule as written.

mod linear;
mod report;
mod run;
pub(crate) mod scan;
mod stream;
#[cfg(test)]
mod tests;

use std::path::{Path, PathBuf};

use tessera_lifecycle::resolve::Batch;
use tessera_spatial::tiler::ScalarType;

use crate::config::{Fields, Schema, ViewSelector};
use crate::error::{BuildError, Result};

pub(crate) use report::describe as describe_refused;
pub use report::RefusedRows;
pub(crate) use linear::number as number_linear;
pub(crate) use stream::number as number_streaming;

/// What a reader hands a pass for a row that names no item in this build: one another view's
/// selection, `--limit` or the rule left out. Every reader skips it.
pub(crate) const NO_SOURCE: u64 = u64::MAX;

/// The column a build file's rows would carry a `tessera_id` in. Nothing holds one in an empty
/// database, so a row carrying one is refused.
pub const TESSERA_ID_COLUMN: &str = "tessera_id";

/// A unique field one file carries.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CarriedField {
    /// The attribute's position in the declaration.
    pub position: u16,
    pub attribute: String,
    /// The column the file holds it in.
    pub column: String,
    pub ty: ScalarType,
}

/// The unique fields a file with this schema carries, under the names `fields` resolved: each
/// attribute declared `unique` whose column the file has. A column the block's `fields` moved an
/// attribute to must be in the file, and its absence is the refusal returned.
pub fn carried_unique(
    schema: &arrow::datatypes::Schema,
    fields: &Fields,
    declared: &Schema,
) -> std::result::Result<Vec<CarriedField>, String> {
    let mut carried = Vec::new();
    for (position, attribute) in declared.attributes.iter().enumerate() {
        if !attribute.unique {
            continue;
        }
        let moved = fields.unique_column(&attribute.name);
        let column = moved.unwrap_or(attribute.column());
        if schema.column_with_name(column).is_none() {
            if moved.is_some() {
                return Err(format!(
                    "{}: `fields.{}` names the column '{column}', which this file does not carry",
                    fields.object(),
                    attribute.name
                ));
            }
            continue;
        }
        carried.push(CarriedField {
            position: position as u16,
            attribute: attribute.name.clone(),
            column: column.to_string(),
            ty: attribute.ty,
        });
    }
    Ok(carried)
}

/// Where a file whose rows address items has no column to address them by: the sentence both the
/// build's refusal and `tessera check`'s finding give.
pub fn no_identifier(object: &str, path: &Path) -> String {
    format!(
        "{object} reads {}, and {}",
        path.display(),
        tessera_lifecycle::resolve::NoIdentifier
    )
}

/// The rows of one file a build reads, as the rule decides them.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum ReadKind {
    /// A view's points, by index into [`crate::BuildArgs::views`].
    Points(usize),
    /// An attribute source that is a file of its own, by index into
    /// [`crate::BuildArgs::attribute_sources`]. A source that is a view's points file is read as
    /// those points' rows.
    Attributes(usize),
    /// A group-scoped attribute's own file, under one view's selection.
    Scoped { family: usize, view: usize },
    /// The access relation every view shares.
    Access,
    /// A layer's `[layer.members]` file, by index into [`crate::BuildArgs::layer_inputs`].
    Members(usize),
    /// The memberships written in a layer's artifact rows, one row per member.
    Lists(usize),
}

/// One file a build reads, what its rows may do, and how the report names it.
pub(crate) struct Read {
    pub kind: ReadKind,
    pub batch: Batch,
    pub input: ReadInput,
    /// The file's name, as the report quotes it.
    pub source: String,
    /// The block reading it, as the report quotes it.
    pub object: String,
}

pub(crate) enum ReadInput {
    File {
        path: PathBuf,
        fields: Fields,
        select: Option<ViewSelector>,
    },
    /// Memberships written in an artifacts file or in the declaration: each member one row.
    Lists(crate::layers::MemberLists),
}

/// Every file this build reads, in declaration order.
pub(crate) fn reads(args: &crate::BuildArgs) -> Result<Vec<Read>> {
    let file_name = |path: &Path| {
        path.file_name()
            .map(|name| name.to_string_lossy().into_owned())
            .unwrap_or_else(|| path.display().to_string())
    };
    let mut reads = Vec::new();
    for (index, view) in args.views.iter().enumerate() {
        reads.push(Read {
            kind: ReadKind::Points(index),
            batch: Batch::Creates,
            input: ReadInput::File {
                path: view.points.clone(),
                fields: view.point_fields.clone(),
                select: view.select.clone(),
            },
            source: file_name(&view.points),
            object: format!("view '{}'", view.view_id),
        });
    }
    for (index, group) in args.attribute_sources.iter().enumerate() {
        if args.views.iter().any(|view| view.points == group.path) {
            continue;
        }
        reads.push(Read {
            kind: ReadKind::Attributes(index),
            batch: Batch::Edits,
            input: ReadInput::File {
                path: group.path.clone(),
                fields: group.fields.clone(),
                select: None,
            },
            source: group.name.clone(),
            object: format!("attribute source '{}'", group.name),
        });
    }
    for (family_index, family) in args.scoped_attributes.iter().enumerate() {
        let Some(source) = &family.source else {
            continue;
        };
        for &view in &family.views {
            reads.push(Read {
                kind: ReadKind::Scoped {
                    family: family_index,
                    view,
                },
                batch: Batch::Edits,
                input: ReadInput::File {
                    path: source.path.clone(),
                    fields: source.fields.clone(),
                    select: Some(crate::scoped_selector(args, family, view)),
                },
                source: file_name(&source.path),
                object: format!(
                    "attribute '{}' in view '{}'",
                    family.attribute.name, args.views[view].view_id
                ),
            });
        }
    }
    if let Some(path) = crate::shared_relation(args)? {
        reads.push(Read {
            kind: ReadKind::Access,
            batch: Batch::Names,
            input: ReadInput::File {
                path: path.to_path_buf(),
                fields: Fields::canonical("point_visibility"),
                select: None,
            },
            source: file_name(path),
            object: "point_visibility".to_string(),
        });
    }
    for (index, input) in args.layer_inputs.iter().enumerate() {
        if let Some(lists) = crate::layers::member_lists(input, &args.schema)? {
            reads.push(Read {
                kind: ReadKind::Lists(index),
                batch: Batch::Names,
                source: lists.source.clone(),
                object: format!("layer '{}' memberships", input.name),
                input: ReadInput::Lists(lists),
            });
        }
        if let Some(members) = &input.members {
            reads.push(Read {
                kind: ReadKind::Members(index),
                batch: Batch::Names,
                input: ReadInput::File {
                    path: members.path.clone(),
                    fields: members.fields.clone(),
                    select: None,
                },
                source: file_name(&members.path),
                object: format!("layer '{}' members", input.name),
            });
        }
    }
    Ok(reads)
}

/// Every row's number in one file: the item it names or creates, or none.
#[derive(Debug)]
pub(crate) enum Numbers {
    /// Every row creates an item, in order: row `r` is item `base + r`.
    Offset { base: u32 },
    /// Row `r`'s number plus one, zero where the row names no item.
    Mapped(crate::spill::MappedArray<u32>),
    /// The same, held in memory.
    Held(Vec<u32>),
}

impl Numbers {
    /// The source ids of rows `first..first + len`, [`NO_SOURCE`] where a row names no item.
    pub(crate) fn extend(&self, first: u64, len: usize, out: &mut Vec<u64>) {
        let stored = match self {
            Numbers::Offset { base } => {
                let start = u64::from(*base) + first;
                out.extend(start..start + len as u64);
                return;
            }
            Numbers::Mapped(rows) => &rows.as_slice()[first as usize..first as usize + len],
            Numbers::Held(rows) => &rows[first as usize..first as usize + len],
        };
        out.extend(
            stored
                .iter()
                .map(|&v| v.checked_sub(1).map_or(NO_SOURCE, u64::from)),
        );
    }

    /// The bytes of disk this file's numbers hold: none where they are a sum.
    pub(crate) fn disk_bytes(&self) -> u64 {
        match self {
            Numbers::Mapped(rows) => rows.as_slice().len() as u64 * 4,
            Numbers::Offset { .. } | Numbers::Held(_) => 0,
        }
    }
}

/// What the rule decided for one file: every row's number, and which row groups hold a numbered
/// row where a `--limit` ruled some out.
#[derive(Debug)]
pub(crate) struct ReadRows {
    pub numbers: Numbers,
    pub groups: Option<Vec<usize>>,
    /// How many of the file's rows name an item, and the order-independent sum of their numbers
    /// ([`crate::spill::mix64`]), for a later pass over the same file to be checked against.
    pub named: u64,
    pub mixed: u64,
}

impl ReadRows {
    /// The row groups a reader decodes out of `total`.
    pub(crate) fn groups(&self, total: usize) -> Vec<usize> {
        match &self.groups {
            Some(groups) => groups.clone(),
            None => (0..total).collect(),
        }
    }
}

/// Every file's numbers, the items the build holds, and what the rule refused.
#[derive(Debug)]
pub(crate) struct Numbering {
    reads: Vec<(ReadKind, ReadRows)>,
    pub items: u64,
    pub refused: Vec<RefusedRows>,
}

impl Numbering {
    /// No file numbered, for a reader exercised over rows that name no item.
    #[cfg(test)]
    pub(crate) fn empty() -> Numbering {
        Numbering {
            reads: Vec::new(),
            items: 0,
            refused: Vec::new(),
        }
    }

    /// One layer's listed members numbered, as stored (the item plus one), and nothing else.
    #[cfg(test)]
    pub(crate) fn with_lists(layer: usize, stored: Vec<u32>) -> Numbering {
        Numbering {
            reads: vec![(
                ReadKind::Lists(layer),
                ReadRows {
                    numbers: Numbers::Held(stored),
                    groups: None,
                    named: 0,
                    mixed: 0,
                },
            )],
            items: 0,
            refused: Vec::new(),
        }
    }

    pub(crate) fn of(&self, kind: ReadKind) -> Option<&ReadRows> {
        self.reads
            .iter()
            .find(|(held, _)| *held == kind)
            .map(|(_, rows)| rows)
    }

    /// A view's points.
    pub(crate) fn points(&self, view: usize) -> &ReadRows {
        self.of(ReadKind::Points(view))
            .expect("every view's points are numbered")
    }

    /// The disk every file's numbers hold until the build ends.
    pub(crate) fn disk_bytes(&self) -> u64 {
        self.reads.iter().map(|(_, rows)| rows.numbers.disk_bytes()).sum()
    }
}

/// `--limit`: the rows whose value of the one unique integer attribute is below a value.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Limit {
    /// The attribute's position in the declaration.
    pub position: u16,
    pub below: u64,
}

impl Limit {
    /// The limit a build of `schema` takes from `--limit below`: over the declaration's one unique
    /// integer attribute, refused where it has none or several.
    pub fn of(schema: &Schema, below: Option<u64>) -> Result<Option<Limit>> {
        let Some(below) = below else {
            return Ok(None);
        };
        let integers: Vec<usize> = schema
            .attributes
            .iter()
            .enumerate()
            .filter(|(_, a)| a.unique && a.ty.integer_range().is_some())
            .map(|(position, _)| position)
            .collect();
        match integers.as_slice() {
            [position] => Ok(Some(Limit {
                position: *position as u16,
                below,
            })),
            _ => Err(BuildError::Invalid(format!(
                "`--limit` keeps the rows whose value of the unique integer attribute is below \
                 it, and this declaration has {} of them. Declare one integer attribute \
                 `unique`, or build the whole corpus",
                integers.len()
            ))),
        }
    }

    /// The column a file keeps the limit's attribute in, where it carries it.
    pub fn column<'a>(&self, carried: &'a [CarriedField]) -> Option<&'a str> {
        carried
            .iter()
            .find(|field| field.position == self.position)
            .map(|field| field.column.as_str())
    }
}
