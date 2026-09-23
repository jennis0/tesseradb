//! A record-bearing body (`/control/ingest`, `/control/values`), in Arrow IPC or JSON, decoded
//! into rows against the manifest's declarations and the layer registry.

mod arrow;
mod json;
mod membership;

use tessera_engine::{DeclaredScalar, ScalarType, ScopedScalar, ABSENT_CODE};
use tessera_lifecycle::WalScalar;
use tessera_types::TesseraId;

use self::arrow::code_at;
pub(crate) use self::arrow::{parse_ingest_batch, parse_values_batch, ParsedBatch, ParsedValues};

/// A body the decoder refuses. A refusal names a row by its index in the batch, never by the id
/// the caller sent.
#[derive(Debug)]
pub(crate) struct DecodeError(pub(crate) String);

/// A column a route gives a meaning of its own, whatever the manifest declares.
#[derive(Clone, Copy)]
pub(crate) enum Fixed<'a> {
    ExternalId,
    /// A coordinate, under the name the view's projection gives its axis.
    Coordinate(&'a str),
    Access,
    NodeId,
    TesseraId,
    IdSet,
}

impl Fixed<'_> {
    pub(crate) fn name(&self) -> &str {
        match self {
            Fixed::ExternalId => "external_id",
            Fixed::Coordinate(name) => name,
            Fixed::Access => "access",
            Fixed::NodeId => "node_id",
            Fixed::TesseraId => "tessera_id",
            Fixed::IdSet => "idset",
        }
    }
}

/// How one item names its entity: the two address forms, already shape-validated.
pub(crate) enum Address {
    External(Vec<u8>),
    Tessera { id: TesseraId, idset: u32 },
}

/// The two encodings a record-bearing route takes; both decode to one row form.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum BodyEncoding {
    Json,
    Arrow,
}

/// The longest external id, in bytes. A longer one is refused, never truncated: truncating
/// would merge two items whose ids share a prefix.
pub(crate) const EXTERNAL_ID_MAX_LEN: usize = 64;

/// A group-scoped family as a [`DeclaredScalar`], so its key check, wire type and code minting
/// are the entity-scoped ones; the scope changes only which column file a value lands in.
pub(crate) fn scoped_as_declared(family: &ScopedScalar) -> DeclaredScalar {
    DeclaredScalar {
        name: family.name.clone(),
        arrow_type: family.arrow_type,
        vocabulary: family.vocabulary.clone(),
        analyser: family.analyser.clone(),
        index: family.index,
        render: family.render,
    }
}

/// [`scoped_as_declared`]'s inverse, so an omitted declared column takes a family's absence.
pub(crate) fn declared_as_scoped(d: &DeclaredScalar) -> ScopedScalar {
    ScopedScalar {
        name: d.name.clone(),
        group: String::new(),
        arrow_type: d.arrow_type,
        vocabulary: d.vocabulary.clone(),
        analyser: d.analyser.clone(),
        index: d.index,
        render: d.render,
        views: Vec::new(),
    }
}

/// A family's wire type: a scoped category arrives as its key, as an entity-scoped one does.
pub(crate) fn scoped_wire_type(family: &ScopedScalar) -> ScalarType {
    scoped_as_declared(family).wire_type()
}

/// The value a row carries for a column the batch omits: a category's reserved code 0, which
/// its vocabulary keeps out of the value space, and `Null` for every other type.
pub(crate) fn scoped_absent(family: &ScopedScalar) -> WalScalar {
    match family.vocabulary {
        Some(_) => code_at(family.arrow_type, ABSENT_CODE),
        None => WalScalar::Null,
    }
}
