//! How a declaration names a row, and what an external id is made of.
//!
//! A build joins every source it reads on one value per row: the column
//! `[defaults].entity_id_field` names, or a view's `fields.entity_id`
//! (`configuration.md` §1, §8). That column may hold an integer, a string or binary bytes, and the
//! bytes it holds are the row's **external id** — contracts §2.4's caller-supplied namespace,
//! taken as supplied and never manufactured.
//!
//! **The ordinal space is the key's rank, so nothing below this module changes.** Every pass in
//! the build joins on a `u64` source id, and pass one's cheapest arrangement is a contiguous range
//! of them (`pipeline::SourceIds`). A supplied key is therefore interned once, before the first
//! pass: the keys of every view's points file are collected, sorted bytewise and deduplicated, and
//! a key's source id is its rank in that order. The ranks are `0..n`, so the union is contiguous
//! by construction and every consumer — the entity-id assignment, the attribute join, the member
//! join, the external-id write — reads the same `u64` it always did.
//!
//! **Sorting by bytes rather than by arrival is what makes the external-id index fall out.** The
//! index is `(external_id, entity_id)` sorted by the id's bytes (contracts §2.4), and a rank walk
//! is already in that order, so the supplied route writes it without a sort. The integer route
//! keeps its own sort: eight little-endian bytes are not in numeric order, which is what
//! `ExternalIdRow`'s byte-swapped key is for.
//!
//! **An id a points file does not carry resolves to [`NO_SOURCE_ID`]**, which no rank can be. A
//! member row naming one is the refusal it is for an unknown integer, and an attribute row naming
//! one is a row the join did not match, counted and reported (`configuration.md` §8).
//!
//! **The keys are held in memory for the length of the build.** A corpus of `n` supplied keys
//! costs their bytes plus a pointer and a length each, which is the one structure this route adds
//! over the integer one. The integer route allocates nothing here at all.

use std::path::Path;

use arrow::array::Array;
use arrow::datatypes::DataType;

use crate::config::ENTITY_ID;
use crate::error::{BuildError, Result};

/// The source id of a row whose key no points file carries.
///
/// `u64::MAX` is not a rank — the ranks are `0..n` and `n` is bounded by the entity-id space — so
/// a consumer's existing "this id is in no view" arm is what answers for it.
pub const NO_SOURCE_ID: u64 = u64::MAX;

/// How this build's declaration names a row.
#[derive(Debug)]
pub enum IdSpace {
    /// The id column holds an integer, which is both the join key and the external id's eight
    /// little-endian bytes. Every test corpus takes this route.
    Integer,
    /// The id column holds supplied keys. A row's join key is the key's rank; its external id is
    /// the key's bytes.
    Supplied(SuppliedIds),
    /// **The points file carries no identity column.** The caller supplied no external id, so the
    /// bundle writes no extent and no locator and a row is addressable by its `tessera_id` alone
    /// (contracts §2.4). A row's join key is its position in the points file, which is why this
    /// route is admitted only where every reader walks that one file whole and in order — see
    /// [`IdSpace::prepare`]'s refusals.
    Positional,
}

/// Every key this build's points files carry, bytewise ascending and unique.
#[derive(Debug)]
pub struct SuppliedIds {
    keys: Vec<Box<[u8]>>,
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
}

impl IdSpace {
    /// Decide the route from the id column's type, and intern the keys where it is a supplied one.
    ///
    /// **Every view's points file must spell identity at the same type.** The views share one
    /// entity space (`views.md` §7), so a string in one and an integer in another would be two key
    /// spaces wearing one column name — a row present in both views under the same identity would
    /// be two entities, with no error anywhere.
    pub fn prepare(args: &crate::BuildArgs) -> Result<IdSpace> {
        if let Some(view) = args.views.first() {
            if !crate::input::has_id_column(&view.points, &view.point_fields)? {
                return positional(args, view);
            }
        }
        let mut spelling: Option<(&crate::ViewArgs, DataType, IdKind)> = None;
        for view in &args.views {
            let found = crate::input::id_column_type(&view.points, &view.point_fields)?;
            let kind = IdKind::of(&found).ok_or_else(|| BuildError::Schema {
                path: view.points.clone(),
                detail: format!(
                    "the identity column '{}' holds {found:?}, which is neither an integer nor \
                     bytes. A row is named by an integer, a string or binary bytes, and whatever \
                     that column holds is the row's external id (configuration.md §8, contracts \
                     §2.4)",
                    view.point_fields.of(ENTITY_ID)
                ),
            })?;
            match &spelling {
                Some((first, held, first_kind)) if *first_kind != kind => {
                    return Err(mixed(first, held, view, &found));
                }
                _ => spelling = Some((view, found, kind)),
            }
        }
        if !matches!(spelling, Some((_, _, IdKind::Bytes))) {
            return Ok(IdSpace::Integer);
        }
        // **`--limit` selects on an integer id and there is no integer here.** It keeps the rows
        // whose identity is below it, which a supplied key has no order for that a caller would
        // recognise. Refused rather than reinterpreted as a rank, which would be a prefix of the
        // corpus nobody asked for.
        if args.limit.is_some() {
            return Err(BuildError::Invalid(
                "`--limit` keeps the rows whose identity is below it, and this declaration's \
                 identity column holds supplied keys rather than integers. Build the whole \
                 corpus, or select the rows in the points file"
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
        // Bytewise, which is the order the external-id index is written in (contracts §2.4), and
        // deduplicated across views: a point has one identity in every view it appears in.
        keys.sort_unstable();
        keys.dedup();
        Ok(IdSpace::Supplied(SuppliedIds { keys }))
    }

    /// The source id `key` names, or [`NO_SOURCE_ID`] where no points file carries it. On the
    /// integer route the eight little-endian bytes are read back as the integer they encode.
    pub fn resolve(&self, key: &[u8]) -> u64 {
        match self {
            IdSpace::Positional => NO_SOURCE_ID,
            IdSpace::Integer => match <[u8; 8]>::try_from(key) {
                Ok(bytes) => u64::from_le_bytes(bytes),
                Err(_) => NO_SOURCE_ID,
            },
            IdSpace::Supplied(keys) => keys.rank(key),
        }
    }

    /// The supplied keys, or `None` on the integer route.
    pub fn supplied(&self) -> Option<&SuppliedIds> {
        match self {
            IdSpace::Integer | IdSpace::Positional => None,
            IdSpace::Supplied(keys) => Some(keys),
        }
    }

    /// Whether a row is named by its position in the points file rather than by a column.
    pub fn positional(&self) -> bool {
        matches!(self, IdSpace::Positional)
    }

    /// The external id a source id carries: the supplied key's own bytes, or the integer's eight
    /// little-endian bytes.
    pub fn external_id(&self, source_id: u64) -> std::borrow::Cow<'_, [u8]> {
        match self {
            IdSpace::Positional | IdSpace::Integer => {
                std::borrow::Cow::Owned(source_id.to_le_bytes().to_vec())
            }
            IdSpace::Supplied(keys) => std::borrow::Cow::Borrowed(keys.key(source_id)),
        }
    }

    /// The `(first, last)` source id the union can hold, where that is known without reading a
    /// row. The supplied route's ranks are `0..n`, so its span is exact.
    pub fn rank_bounds(&self) -> Option<(u64, u64)> {
        match self {
            IdSpace::Integer | IdSpace::Positional => None,
            IdSpace::Supplied(keys) if keys.is_empty() => None,
            IdSpace::Supplied(keys) => Some((0, keys.len() as u64 - 1)),
        }
    }
}

/// The positional route, and the four shapes it refuses.
///
/// **A row is named by its position, so every reader must walk the same file whole and in the same
/// order.** Nothing else can be joined to it: a second file's rows are its own, a `--limit` prunes
/// row groups before they are counted, and a view's selection keeps some rows and not others. Each
/// of those is refused here rather than resolved to a position that means something else — a join
/// under the wrong identity puts one row's attributes, and one row's access terms, under another
/// row's `tessera_id`, with no error anywhere.
fn positional(args: &crate::BuildArgs, view: &crate::ViewArgs) -> Result<IdSpace> {
    let refuse = |detail: String| -> Result<IdSpace> {
        Err(BuildError::Invalid(format!(
            "{}: the points file carries no column named '{}', so each row is named by its \
             position in it and the bundle mints no external ids (contracts §2.4). {detail}",
            view.points.display(),
            view.point_fields.of(ENTITY_ID)
        )))
    };
    for other in &args.views {
        if !crate::input::has_id_column(&other.points, &other.point_fields)? {
            continue;
        }
        return refuse(format!(
            "View '{}' does carry one, and a position in one file names no row of another. \
             Declare the identity column on every view, or on none",
            other.view_id
        ));
    }
    if args.views.len() > 1 || view.select.is_some() {
        return refuse(
            "This build materialises several views, whose rows are several files' or a selection \
             of one file's. Positions name rows of one whole file. Declare an identity column"
                .to_string(),
        );
    }
    if args.limit.is_some() {
        return refuse(
            "`--limit` keeps the rows whose identity is below it, and a position is not an \
             identity the caller wrote. Build the whole corpus"
                .to_string(),
        );
    }
    if let crate::config::AccessSource::Relation(path) = &view.access.source {
        return refuse(format!(
            "The access relation {} names each point's terms by its entity id, and there is none \
             to name. Read the labels from a column of the points file, or declare an identity \
             column",
            path.display()
        ));
    }
    for group in &args.attribute_sources {
        if group.path != view.points {
            return refuse(format!(
                "Attribute source {} is a second file, whose rows are joined by the identity they \
                 name. Read the columns from the points file, or declare an identity column",
                group.path.display()
            ));
        }
    }
    for input in &args.layer_inputs {
        if let Some(members) = &input.members {
            return refuse(format!(
                "Layer '{}' reads its members from {}, one row per (artifact, entity), and there \
                 is no entity to name. Declare an identity column",
                input.name,
                members.path.display()
            ));
        }
    }
    Ok(IdSpace::Positional)
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
            "view '{}' spells identity at {found:?} and view '{}' spells it at {held:?}. The \
             views share one entity space (views §7), so one column at two types is two key \
             spaces under one name and a row in both views would be two entities",
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
            DataType::UInt64 | DataType::UInt32 | DataType::Int64 | DataType::Int32 => {
                Some(IdKind::Integer)
            }
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

/// One row's key from an identity column of any accepted type, or `None` where the row is null.
///
/// **An integer is read as its eight little-endian bytes**, which is what the integer route writes
/// as an external id, so a corpus that spells its ids one way in the points file and the other way
/// in a members file joins on the same bytes.
pub fn key_at(column: &dyn Array, row: usize) -> Option<Vec<u8>> {
    use arrow::array::{
        BinaryArray, BinaryViewArray, Int32Array, Int64Array, LargeBinaryArray, LargeStringArray,
        StringArray, StringViewArray, UInt32Array, UInt64Array,
    };
    if column.is_null(row) {
        return None;
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
    macro_rules! integer {
        ($ty:ty) => {
            if let Some(values) = any.downcast_ref::<$ty>() {
                return Some((values.value(row) as u64).to_le_bytes().to_vec());
            }
        };
    }
    bytes!(StringArray);
    bytes!(LargeStringArray);
    bytes!(StringViewArray);
    raw!(BinaryArray);
    raw!(LargeBinaryArray);
    raw!(BinaryViewArray);
    integer!(UInt64Array);
    integer!(UInt32Array);
    integer!(Int64Array);
    integer!(Int32Array);
    None
}

/// One row's identity as a refusal should print it: an integer as its number, supplied bytes as
/// their text, and a row the column cannot be read at as its position in the file.
pub fn display_at(column: &dyn Array, row: usize) -> String {
    use arrow::array::{Int32Array, Int64Array, UInt32Array, UInt64Array};
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
        if let Some(key) = key_at(column, row) {
            return String::from_utf8_lossy(&key).into_owned();
        }
    }
    format!("row {row}")
}

/// Refuse a null in the column a row's identity is read from.
pub fn null_id(path: &Path, name: &str) -> BuildError {
    BuildError::Schema {
        path: path.to_path_buf(),
        detail: format!(
            "the identity column '{name}' has a null in it, and a row that names no entity has no \
             identity. Every row is named by the bytes this column holds (contracts §2.4)"
        ),
    }
}

/// Whether this build writes the external-id index and the locator (contracts §2.4).
///
/// **A supplied id column is an external id, so it is written without a flag.** The caller named
/// every row, and contracts §2.4's rule is that an item's identifier is the caller's where the
/// caller supplies one. `--mint-external-ids` keeps its meaning for the other route: an integer
/// `entity_id` column is a source-corpus number rather than a namespace the caller owns, so
/// minting one is opt-in and a build that does not ask writes no sidecar at all.
pub fn writes_external_ids(args: &crate::BuildArgs, ids: &IdSpace) -> bool {
    if ids.positional() {
        // Nothing in the build or the ingest path may manufacture an external id for an item that
        // has none (contracts §2.4), and a position is not one.
        return false;
    }
    args.mint_external_ids || ids.supplied().is_some()
}
