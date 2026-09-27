//! How a declaration names a row: the join field every source file joins on.
//!
//! `[defaults].join_field` names a declared unique attribute, and every file a build reads rows
//! of — each view's points, each attribute source, each layer's members, an access relation —
//! names its item by that field's value, in the column the block's `fields` resolve to it
//! (`crate::config::JOIN_COLUMN`). The column holds an integer or a string.
//!
//! **The ordinal space is the key's rank, so nothing below this module changes.** Every pass in
//! the build joins on a `u64` source id, and pass one's cheapest arrangement is a contiguous range
//! of them (`pipeline::SourceIds`). An integer is its own source id. A string is interned once,
//! before the first pass: the keys of every view's points file are collected, sorted bytewise and
//! deduplicated, and a key's source id is its rank in that order. The ranks are `0..n`, so the
//! union is contiguous and every consumer reads the same `u64`: the entity-id assignment, the
//! attribute join and the member join.
//!
//! **A value a points file does not carry resolves to [`NO_SOURCE_ID`]**, which no rank can be. A
//! member row naming one is a refusal naming the value, and an attribute row naming one is a row
//! the join did not match, counted and reported (`configuration.md` §8).
//!
//! ## What the string route costs
//!
//! **The keys are resident for the length of the build, and there is no spill route for them.**
//! A key costs its `Box<[u8]>` in the interned vector, 16 bytes, plus its own heap allocation,
//! which glibc rounds to a 16-byte chunk with an 8-byte header and a 32-byte floor. Modelled, not
//! measured: at a 20-byte mean key that is 16 + 32 = 48 B/item, so 10⁸ keys are 4.5 GiB and 10⁹
//! are 45. `residency::disk` charges it as a named term, so the pre-flight refuses a build that
//! would not fit rather than the kernel killing it. The integer route allocates nothing here.

use std::path::Path;

use arrow::array::{Array, Int32Array, Int64Array, UInt32Array, UInt64Array};
use arrow::datatypes::DataType;

use crate::config::JOIN_COLUMN;
use crate::error::{BuildError, Result};

/// The source id of a row whose key no points file carries.
///
/// `u64::MAX` is not a rank: the ranks are `0..n` and `n` is bounded by the entity-id space, so a
/// consumer's existing "this id is in no view" arm is what answers for it.
pub const NO_SOURCE_ID: u64 = u64::MAX;

/// How this build's declaration names a row.
#[derive(Debug)]
pub enum IdSpace {
    /// The join column holds an integer, which is the row's source id. `signed` where the column
    /// is a signed type, whose values the source ids hold as their two's-complement bits.
    Integer { signed: bool },
    /// The join column holds strings. A row's source id is its key's rank.
    Supplied(SuppliedIds),
    /// **The declaration names no join field.** A row's source id is its position in the points
    /// file, which is why this route is admitted only where every reader walks that one file
    /// whole and in order. The refusals that hold it to that are in [`Addressing`].
    Positional,
}

/// Every key this build's points files carry, bytewise ascending and unique, and the Arrow type
/// the declaration spells them at.
#[derive(Debug)]
pub struct SuppliedIds {
    keys: Vec<Box<[u8]>>,
    /// The points file's own identity column type, for the refusal a second source earns when it
    /// spells identity in the other family.
    spelled: DataType,
}

impl SuppliedIds {
    /// The source id of `key`, or [`NO_SOURCE_ID`] where no points file carries it.
    pub fn rank(&self, key: &[u8]) -> u64 {
        match self.keys.binary_search_by(|held| held.as_ref().cmp(key)) {
            Ok(rank) => rank as u64,
            Err(_) => NO_SOURCE_ID,
        }
    }

    /// The key a source id was interned from.
    pub fn key(&self, source_id: u64) -> &[u8] {
        &self.keys[source_id as usize]
    }

    pub fn len(&self) -> usize {
        self.keys.len()
    }

    /// Whether this build's points files named no row at all.
    pub fn is_empty(&self) -> bool {
        self.keys.is_empty()
    }

    /// The mean key length in bytes, which is what the pre-flight charges the arena and the
    /// sidecar at (module doc). Zero over no keys.
    pub fn mean_key_len(&self) -> u64 {
        if self.keys.is_empty() {
            return 0;
        }
        let total: usize = self.keys.iter().map(|key| key.len()).sum();
        (total / self.keys.len()) as u64
    }

    /// **A second source must hold the join value in the same family**, and the refusal is made
    /// once against the column rather than once per row.
    ///
    /// An integer column read against interned keys resolves every row to an unknown key: a
    /// members table would refuse row by row on the first one, and an attribute source would
    /// report zero coverage over a join that was never going to match. Both are answers to the
    /// wrong question.
    pub fn require_same_family(
        &self,
        path: &Path,
        object: &str,
        column: &str,
        found: &DataType,
    ) -> Result<()> {
        if IdKind::of(found) == Some(IdKind::Bytes) {
            return Ok(());
        }
        Err(BuildError::Schema {
            path: path.to_path_buf(),
            detail: format!(
                "{object}: the column '{column}' naming each row holds {found:?}, and this \
                 declaration's points file holds the join field at {:?}. Write the join value at \
                 the points file's type",
                self.spelled
            ),
        })
    }
}

impl IdSpace {
    /// Decide the route from the id column's type, and intern the keys where it is a supplied one.
    ///
    /// **Every view's points file must spell identity at the same type.** The views share one
    /// entity space (`views.md` §7), so a string in one and an integer in another would be two key
    /// spaces wearing one column name, and a row present in both views under the same identity
    /// would be two entities with no error anywhere.
    pub fn prepare(args: &crate::BuildArgs) -> Result<IdSpace> {
        if let Some(view) = args.views.first() {
            if !view.point_fields.joins() {
                return positional(args, view);
            }
        }
        let mut spelling: Option<(&crate::ViewArgs, DataType, IdKind)> = None;
        for view in &args.views {
            let found = crate::input::id_column_type(&view.points, &view.point_fields)?;
            let kind = IdKind::of(&found).ok_or_else(|| BuildError::Schema {
                path: view.points.clone(),
                detail: format!(
                    "the join column '{}' holds {found:?}, which is neither an integer nor a \
                     string. Join on a `keyword` or an integer column",
                    view.point_fields.of(JOIN_COLUMN)
                ),
            })?;
            match &spelling {
                Some((first, held, first_kind)) if *first_kind != kind => {
                    return Err(mixed(first, held, view, &found));
                }
                _ => spelling = Some((view, found, kind)),
            }
        }
        let spelled = match spelling {
            Some((_, spelled, IdKind::Bytes)) => spelled,
            Some((_, found, IdKind::Integer)) => {
                return Ok(IdSpace::Integer {
                    signed: found.is_signed_integer(),
                })
            }
            None => return Ok(IdSpace::Integer { signed: false }),
        };
        // **`--limit` selects on an integer id and there is no integer here.** It keeps the rows
        // whose identity is below it, and a supplied key has no order a caller would recognise it
        // by. Refused rather than reinterpreted as a rank, which would be a prefix of the corpus
        // nobody asked for.
        if args.limit.is_some() {
            return Err(BuildError::Invalid(
                "`--limit` keeps the rows whose join value is below it, and this declaration's \
                 join field is a string. Build the whole corpus, or select the rows in the points \
                 file"
                    .to_string(),
            ));
        }
        let mut keys: Vec<Box<[u8]>> = Vec::new();
        for view in &args.views {
            crate::input::scan_id_keys(
                &view.points,
                &view.point_fields,
                view.select.as_ref(),
                |key| keys.push(key.into()),
            )?;
        }
        // Deduplicated across views: a point has one identity in every view it appears in.
        keys.sort_unstable();
        keys.dedup();
        Ok(IdSpace::Supplied(SuppliedIds { keys, spelled }))
    }

    /// The supplied keys, or `None` on the integer and positional routes.
    pub fn supplied(&self) -> Option<&SuppliedIds> {
        match self {
            IdSpace::Integer { .. } | IdSpace::Positional => None,
            IdSpace::Supplied(keys) => Some(keys),
        }
    }

    /// Whether a row is named by its position in the points file rather than by a column.
    pub fn positional(&self) -> bool {
        matches!(self, IdSpace::Positional)
    }

    /// How the points file names the item a source id stands for: the integer, the supplied key,
    /// or the row it sits at.
    pub fn display(&self, source_id: u64) -> String {
        match self {
            IdSpace::Integer { signed: false } => source_id.to_string(),
            IdSpace::Integer { signed: true } => (source_id as i64).to_string(),
            IdSpace::Supplied(keys) => String::from_utf8_lossy(keys.key(source_id)).into_owned(),
            IdSpace::Positional => format!("at row {source_id}"),
        }
    }

    /// The `(first, last)` source id the union can hold, where that is known without reading a
    /// row. The supplied route's ranks are `0..n`, so its span is exact.
    pub fn rank_bounds(&self) -> Option<(u64, u64)> {
        match self {
            IdSpace::Integer { .. } | IdSpace::Positional => None,
            IdSpace::Supplied(keys) if keys.is_empty() => None,
            IdSpace::Supplied(keys) => Some((0, keys.len() as u64 - 1)),
        }
    }
}

/// What in a declaration names a row by its identity, where a view's points file carries no
/// identity column.
///
/// **A row is then named by its position, so every reader must walk the same file whole and in the
/// same order.** Nothing else can be joined to it. A second file's rows are its own, a `--limit`
/// prunes row groups before they are counted, a view's selection keeps some rows and not others,
/// and a membership written beside an artifact names rows of a file it does not index. Each is
/// named here rather than resolved to a position that means something else: a join under the wrong
/// identity puts one row's attributes, and one row's access terms, under another row's
/// `tessera_id`, with no error anywhere.
///
/// **The build and `tessera check` ask this one question.** A build refuses what
/// [`Addressing::needs_identity`] names and takes [`IdSpace::Positional`] where it names nothing;
/// a check reports the same sentence as a finding against the view, and a declaration it leaves
/// clean is one the build accepts (`check.rs`).
pub struct Addressing<'a> {
    /// The view's points file. An attribute source is that same file or is a second one.
    pub points: &'a Path,
    /// Whether the build materialises more than one view.
    pub several_views: bool,
    /// Whether the view keeps a selection of its file's rows rather than the whole file.
    pub selection: bool,
    /// Whether a `--limit` prunes the corpus.
    pub limit: bool,
    /// The exploded `(entity_id, term_id)` relation the view reads its points' access terms from,
    /// where it declares one.
    pub visibility_source: Option<&'a Path>,
    /// The attribute sources, grouped one per file.
    pub attribute_sources: &'a [crate::config::AttributeSource],
    /// The layer declarations, for the membership each one is written under.
    pub layers: &'a [tessera_types::layer::LayerDeclaration],
    /// Where each layer's artifacts and members come from.
    pub layer_inputs: &'a [crate::config::LayerSources],
}

impl Addressing<'_> {
    /// The first thing in the declaration that names a row by its identity, as a sentence saying
    /// what to declare instead, or `None` where a position is an identity.
    pub fn needs_identity(&self) -> Result<Option<String>> {
        if self.several_views || self.selection {
            return Ok(Some(
                "This build materialises several views, whose rows are several files' or a \
                 selection of one file's. Positions name rows of one whole file. Declare a \
                 `[defaults].join_field`"
                    .to_string(),
            ));
        }
        if self.limit {
            return Ok(Some(
                "`--limit` keeps the rows whose join value is below it, and a position is not a \
                 value the caller wrote. Build the whole corpus"
                    .to_string(),
            ));
        }
        if let Some(path) = self.visibility_source {
            return Ok(Some(format!(
                "The access relation {} names each point's terms by its join value, and there is \
                 none to name. Read the labels from a column of the points file, or declare a \
                 `[defaults].join_field`",
                path.display()
            )));
        }
        for group in self.attribute_sources {
            if group.path != self.points {
                return Ok(Some(format!(
                    "Attribute source {} is a second file, whose rows are joined by the value \
                     they name. Read the columns from the points file, or declare a \
                     `[defaults].join_field`",
                    group.path.display()
                )));
            }
        }
        for input in self.layer_inputs {
            if let Some(members) = &input.members {
                return Ok(Some(format!(
                    "Layer '{}' reads its members from {}, one row per (artifact, entity), and \
                     there is no entity to name. Declare a `[defaults].join_field`",
                    input.name,
                    members.path.display()
                )));
            }
            // **The membership written beside an artifact names rows too**, as a list per artifact
            // rather than a row per member (`configuration.md` §8's two shapes). Read against
            // positions it would publish whichever rows the file happened to be ordered by.
            let enumerated = self.layers.iter().any(|d| {
                d.name == input.name
                    && d.membership == tessera_types::layer::MembershipSource::Enumerated
            });
            if !enumerated {
                continue;
            }
            match &input.artifacts {
                None => {}
                Some(crate::config::ArtifactSource::Inline(rows)) => {
                    if rows
                        .iter()
                        .any(|row| row.members.is_some() || row.excluding.is_some())
                    {
                        return Ok(Some(format!(
                            "Layer '{}' writes its artifacts' memberships inline, and a \
                             membership names entities. Declare a `[defaults].join_field`",
                            input.name
                        )));
                    }
                }
                Some(crate::config::ArtifactSource::File { path, fields, .. }) => {
                    let named = [fields.of("members"), fields.of("excluding")];
                    if let Some(column) = crate::input::first_column_present(path, &named)? {
                        return Ok(Some(format!(
                            "Layer '{}' reads its artifacts from {}, which carries a '{column}' \
                             column, and a membership names entities. Declare a `[defaults].join_field`",
                            input.name,
                            path.display()
                        )));
                    }
                }
            }
        }
        Ok(None)
    }
}

/// The positional route: [`Addressing`]'s question over this build's arguments, refused where it
/// names something.
fn positional(args: &crate::BuildArgs, view: &crate::ViewArgs) -> Result<IdSpace> {
    let refuse = |detail: String| -> Result<IdSpace> {
        Err(BuildError::Invalid(format!(
            "{}: the declaration names no join field, so each row of the points file is an item \
             of its own. {detail}",
            view.points.display(),
        )))
    };
    if let Some(other) = args.views.iter().find(|other| other.point_fields.joins()) {
        return refuse(format!(
            "View '{}' joins on a column, and a position in one file names no row of another. \
             Join every view, or none",
            other.view_id
        ));
    }
    let addressing = Addressing {
        points: &view.points,
        several_views: args.views.len() > 1,
        selection: view.select.is_some(),
        limit: args.limit.is_some(),
        visibility_source: match &view.access.source {
            crate::config::AccessSource::Relation(path) => Some(path),
            _ => None,
        },
        attribute_sources: &args.attribute_sources,
        layers: &args.layers,
        layer_inputs: &args.layer_inputs,
    };
    match addressing.needs_identity()? {
        Some(detail) => refuse(detail),
        None => Ok(IdSpace::Positional),
    }
}

fn mixed(
    first: &crate::ViewArgs,
    held: &DataType,
    view: &crate::ViewArgs,
    found: &DataType,
) -> BuildError {
    BuildError::Schema {
        path: view.points.clone(),
        detail: format!(
            "view '{}' holds the join field at {found:?} and view '{}' holds it at {held:?}. \
             The views share one entity space, so a row in both views would be two items. Write \
             the join value at one type in every view",
            view.view_id, first.view_id
        ),
    }
}

/// The two families an identity column may belong to.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum IdKind {
    Integer,
    Bytes,
}

impl IdKind {
    fn of(ty: &DataType) -> Option<IdKind> {
        match ty {
            ty if is_integer_id(ty) => Some(IdKind::Integer),
            DataType::Utf8
            | DataType::LargeUtf8
            | DataType::Utf8View
            | DataType::Binary
            | DataType::LargeBinary
            | DataType::BinaryView => Some(IdKind::Bytes),
            _ => None,
        }
    }
}

/// Whether a join column at `ty` holds integers.
pub fn is_integer_id(ty: &DataType) -> bool {
    ty.is_integer()
}

/// An integer join column as its ids, a null staying null, or `None` where the column is not
/// one [`is_integer_id`] takes.
///
/// **A signed value is sign-extended to 64 bits and its bits read as unsigned**, so a negative
/// value and its two's-complement unsigned value are the same id. Only one type is read in one
/// build ([`IdSpace::prepare`]), so the two never meet.
pub fn integer_ids(column: &dyn Array) -> Option<UInt64Array> {
    use arrow::array::{Int16Array, Int8Array, UInt16Array, UInt8Array};
    let any = column.as_any();
    if let Some(ids) = any.downcast_ref::<UInt8Array>() {
        return Some(ids.unary(u64::from));
    }
    if let Some(ids) = any.downcast_ref::<UInt16Array>() {
        return Some(ids.unary(u64::from));
    }
    if let Some(ids) = any.downcast_ref::<Int8Array>() {
        return Some(ids.unary(|id| i64::from(id) as u64));
    }
    if let Some(ids) = any.downcast_ref::<Int16Array>() {
        return Some(ids.unary(|id| i64::from(id) as u64));
    }
    if let Some(ids) = any.downcast_ref::<UInt64Array>() {
        return Some(ids.clone());
    }
    if let Some(ids) = any.downcast_ref::<UInt32Array>() {
        return Some(ids.unary(u64::from));
    }
    if let Some(ids) = any.downcast_ref::<Int64Array>() {
        return Some(ids.unary(|id| id as u64));
    }
    if let Some(ids) = any.downcast_ref::<Int32Array>() {
        return Some(ids.unary(|id| i64::from(id) as u64));
    }
    None
}

/// One row's key from a join column of any accepted type, or `None` where the row is null. An
/// integer is read as its id's eight little-endian bytes ([`integer_ids`]).
pub fn key_at(column: &dyn Array, row: usize) -> Option<Vec<u8>> {
    use arrow::array::{
        BinaryArray, BinaryViewArray, LargeBinaryArray, LargeStringArray, StringArray,
        StringViewArray,
    };
    if column.is_null(row) {
        return None;
    }
    if is_integer_id(column.data_type()) {
        let id = integer_ids(&column.slice(row, 1))?;
        return Some(id.value(0).to_le_bytes().to_vec());
    }
    let any = column.as_any();
    macro_rules! bytes {
        ($ty:ty) => {
            if let Some(values) = any.downcast_ref::<$ty>() {
                return Some(values.value(row).as_bytes().to_vec());
            }
        };
    }
    macro_rules! raw {
        ($ty:ty) => {
            if let Some(values) = any.downcast_ref::<$ty>() {
                return Some(values.value(row).to_vec());
            }
        };
    }
    bytes!(StringArray);
    bytes!(LargeStringArray);
    bytes!(StringViewArray);
    raw!(BinaryArray);
    raw!(LargeBinaryArray);
    raw!(BinaryViewArray);
    None
}

/// One row's join value as a refusal should print it: an integer as its number, a string as its
/// text, and a row the column cannot be read at as its position in the file.
pub fn display_at(column: &dyn Array, row: usize) -> String {
    let any = column.as_any();
    macro_rules! integer {
        ($ty:ty) => {
            if let Some(values) = any.downcast_ref::<$ty>() {
                return values.value(row).to_string();
            }
        };
    }
    if !column.is_null(row) {
        integer!(UInt64Array);
        integer!(UInt32Array);
        integer!(Int64Array);
        integer!(Int32Array);
        integer!(arrow::array::UInt16Array);
        integer!(arrow::array::UInt8Array);
        integer!(arrow::array::Int16Array);
        integer!(arrow::array::Int8Array);
        if let Some(key) = key_at(column, row) {
            return String::from_utf8_lossy(&key).into_owned();
        }
    }
    format!("row {row}")
}

/// Refuse a null in the column a row's join value is read from.
pub fn null_id(path: &Path, name: &str) -> BuildError {
    BuildError::Schema {
        path: path.to_path_buf(),
        detail: format!(
            "the join column '{name}' has a null in it, and a row whose join value is null names \
             no item. Give every row a value"
        ),
    }
}

/// Refuse a view whose points name one join value on two rows, naming how many values are held
/// more than once and up to ten of them. `sorted` is the view's source ids, ascending.
///
/// A row is unique per `(join value, view)`: the same value in two views' points is one item in
/// both, and is not refused.
pub(crate) fn refuse_duplicates(
    view: &crate::ViewArgs,
    ids: &IdSpace,
    sorted: impl IntoIterator<Item = u64>,
) -> Result<()> {
    let mut previous: Option<u64> = None;
    let mut counted: Option<u64> = None;
    let mut values = 0u64;
    let mut shown = Vec::new();
    for id in sorted {
        if previous == Some(id) && counted != Some(id) {
            counted = Some(id);
            values += 1;
            if shown.len() < 10 {
                shown.push(ids.display(id));
            }
        }
        previous = Some(id);
    }
    if values == 0 {
        return Ok(());
    }
    Err(BuildError::Invalid(format!(
        "view '{}': {values} join value(s) name more than one row of {}: {}. Each row of one \
         view's points is a different item; give each its own value",
        view.view_id,
        view.points.display(),
        shown.join(", ")
    )))
}
