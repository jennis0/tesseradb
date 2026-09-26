//! A record-bearing body (`/control/ingest`), in Arrow IPC or JSON, decoded
//! into rows against the manifest's declarations and the layer registry.

mod arrow;
mod json;
mod membership;

use tessera_engine::{DeclaredScalar, ScalarType, ScopedScalar};
use tessera_types::TesseraId;

pub(crate) use self::arrow::{labels_col, parse_ingest_batch, Frame, ParsedBatch};

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
}

impl Fixed<'_> {
    pub(crate) fn name(&self) -> &str {
        match self {
            Fixed::ExternalId => "external_id",
            Fixed::Coordinate(name) => name,
            Fixed::Access => "access",
            Fixed::NodeId => "node_id",
            Fixed::TesseraId => "tessera_id",
        }
    }
}

/// How one item names its entity: the two address forms, already shape-validated.
pub(crate) enum Address {
    External(Vec<u8>),
    Tessera(TesseraId),
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
        unique: false,
    }
}

/// A family's wire type: a scoped category arrives as its key, as an entity-scoped one does.
pub(crate) fn scoped_wire_type(family: &ScopedScalar) -> ScalarType {
    scoped_as_declared(family).wire_type()
}
