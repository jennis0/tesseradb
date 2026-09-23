//! How a declaration names a row, and what an external id is made of.
//!
//! A build joins every source it reads on one value per row: the column
//! `[defaults].entity_id_field` names, or a view's `fields.entity_id` (`configuration.md` §1, §8).
//! That column may hold an integer, a string or binary bytes. Whatever it holds is the row's
//! **external id**, contracts §2.4's caller-supplied namespace, taken as supplied and never
//! manufactured.
//!
//! **The ordinal space is the key's rank, so nothing below this module changes.** Every pass in
//! the build joins on a `u64` source id, and pass one's cheapest arrangement is a contiguous range
//! of them (`pipeline::SourceIds`). A supplied key is therefore interned once, before the first
//! pass: the keys of every view's points file are collected, sorted bytewise and deduplicated, and
//! a key's source id is its rank in that order. The ranks are `0..n`, so the union is contiguous
//! and every consumer reads the same `u64` it always did: the entity-id assignment, the attribute
//! join, the member join, the external-id write.
//!
//! **Sorting by bytes rather than by arrival is what makes the external-id index fall out.** The
//! index is `(external_id, entity_id)` sorted by the id's bytes (contracts §2.4), and a rank walk
//! is already in that order, so the supplied route writes it without a sort. The integer route
//! keeps its own sort, eight little-endian bytes not being in the integer's order, which is what
//! `ExternalIdRow`'s byte-swapped key is for.
//!
//! **An id a points file does not carry resolves to [`NO_SOURCE_ID`]**, which no rank can be. A
//! member row naming one is a refusal naming the key, and an attribute row naming one is a row the
//! join did not match, counted and reported (`configuration.md` §8).
//!
//! ## What the supplied route costs
//!
//! **The keys are resident for the length of the build, and there is no spill route for them.**
//! A key costs its `Box<[u8]>` in the interned vector, 16 bytes, plus its own heap allocation,
//! which glibc rounds to a 16-byte chunk with an 8-byte header and a 32-byte floor. Modelled, not
//! measured: at a 20-byte mean key that is 16 + 32 = 48 B/item, so 10⁸ keys are 4.5 GiB and 10⁹
//! are 45. `residency::disk` charges it as a named term, so the pre-flight refuses a build that
//! would not fit rather than the kernel killing it. The integer route allocates nothing here.
//!
//! The sidecar the same route writes is charged beside it, at `12 + mean` bytes an item: a 4-byte
//! Arrow offset, the key's own bytes, a 4-byte entity, a 4-byte locator slot, and a quarter byte
//! an item of Arrow framing.

use std::path::Path;

use arrow::array::{Array, Int32Array, Int64Array, UInt32Array, UInt64Array};
use arrow::datatypes::DataType;

use crate::config::ENTITY_ID;
use crate::error::{BuildError, Result};

/// The source id of a row whose key no points file carries.
///
/// `u64::MAX` is not a rank: the ranks are `0..n` and `n` is bounded by the entity-id space, so a
/// consumer's existing "this id is in no view" arm is what answers for it.
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
    /// route is admitted only where every reader walks that one file whole and in order. The
    /// refusals that hold it to that are in [`IdSpace::prepare`].
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

    /// **A second source must spell identity in the same family**, and the refusal is made once
    /// against the column rather than once per row.
    ///
    /// An integer column read against interned keys resolves every row to an unknown key: a
    /// members table would refuse row by row on the first one, and an attribute source would
    /// report zero coverage over a join that was never going to match. Both are answers to the
    /// wrong question. The declaration named one identity, and a column in the other family is a
    /// producer that has not been rewritten yet.
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
                 declaration's points file spells identity at {:?}. One identity is one type \
                 (configuration.md §8): a column in the other family names none of the rows this \
                 build loaded",
                self.spelled
            ),
        })
    }
}

/// An item's external id, as it goes into the index (contracts §2.4).
///
/// An owning variant for the integer route so that route allocates nothing per row, and a borrow
/// for the supplied one so its keys are written straight out of the arena.
pub enum ExternalId<'a> {
    Integer([u8; 8]),
    Supplied(&'a [u8]),
}

impl AsRef<[u8]> for ExternalId<'_> {
    fn as_ref(&self) -> &[u8] {
        match self {
            ExternalId::Integer(bytes) => bytes,
            ExternalId::Supplied(key) => key,
        }
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
        let Some((_, spelled, IdKind::Bytes)) = spelling else {
            return Ok(IdSpace::Integer);
        };
        // **`--limit` selects on an integer id and there is no integer here.** It keeps the rows
        // whose identity is below it, and a supplied key has no order a caller would recognise it
        // by. Refused rather than reinterpreted as a rank, which would be a prefix of the corpus
        // nobody asked for.
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
        Ok(IdSpace::Supplied(SuppliedIds { keys, spelled }))
    }

    /// The supplied keys, or `None` on the integer and positional routes.
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
    ///
    /// The positional route reaches this nowhere, [`writes_external_ids`] being false for it.
    pub fn external_id(&self, source_id: u64) -> ExternalId<'_> {
        match self.supplied() {
            Some(keys) => ExternalId::Supplied(keys.key(source_id)),
            None => ExternalId::Integer(source_id.to_le_bytes()),
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
                 selection of one file's. Positions name rows of one whole file. Declare an \
                 identity column"
                    .to_string(),
            ));
        }
        if self.limit {
            return Ok(Some(
                "`--limit` keeps the rows whose identity is below it, and a position is not an \
                 identity the caller wrote. Build the whole corpus"
                    .to_string(),
            ));
        }
        if let Some(path) = self.visibility_source {
            return Ok(Some(format!(
                "The access relation {} names each point's terms by its entity id, and there is \
                 none to name. Read the labels from a column of the points file, or declare an \
                 identity column",
                path.display()
            )));
        }
        for group in self.attribute_sources {
            if group.path != self.points {
                return Ok(Some(format!(
                    "Attribute source {} is a second file, whose rows are joined by the identity \
                     they name. Read the columns from the points file, or declare an identity \
                     column",
                    group.path.display()
                )));
            }
        }
        for input in self.layer_inputs {
            if let Some(members) = &input.members {
                return Ok(Some(format!(
                    "Layer '{}' reads its members from {}, one row per (artifact, entity), and \
                     there is no entity to name. Declare an identity column",
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
                             membership names entities. Declare an identity column",
                            input.name
                        )));
                    }
                }
                Some(crate::config::ArtifactSource::File { path, fields, .. }) => {
                    let named = [fields.of("members"), fields.of("excluding")];
                    if let Some(column) = crate::input::first_column_present(path, &named)? {
                        return Ok(Some(format!(
                            "Layer '{}' reads its artifacts from {}, which carries a '{column}' \
                             column, and a membership names entities. Declare an identity column",
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
            "{}: the points file carries no column named '{}', so each row is named by its \
             position in it and the bundle mints no external ids (contracts §2.4). {detail}",
            view.points.display(),
            view.point_fields.of(ENTITY_ID)
        )))
    };
    // **Which view carries one is worth naming**, and only a build knows: a check reads the same
    // several-views sentence out of [`Addressing`].
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

/// Whether an identity column at `ty` holds integers.
pub fn is_integer_id(ty: &DataType) -> bool {
    matches!(
        ty,
        DataType::UInt64 | DataType::UInt32 | DataType::Int64 | DataType::Int32
    )
}

/// An integer identity column as its ids, a null staying null, or `None` where the column is not
/// one [`is_integer_id`] takes.
///
/// **An integer id is its value's eight little-endian bytes, so a negative id and its
/// two's-complement unsigned value are the same id.** A signed value is sign-extended to 64 bits
/// and its bits read as unsigned, which is what the running service and the clients store.
pub fn integer_ids(column: &dyn Array) -> Option<UInt64Array> {
    let any = column.as_any();
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

/// One row's key from an identity column of any accepted type, or `None` where the row is null.
///
/// **An integer is read as its id's eight little-endian bytes** ([`integer_ids`]), which is what
/// the integer route writes as an external id.
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

/// One row's identity as a refusal should print it: an integer as its number, supplied bytes as
/// their text, and a row the column cannot be read at as its position in the file.
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
