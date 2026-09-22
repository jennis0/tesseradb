mod arrow;
mod json;
mod membership;

use tessera_engine::{DeclaredScalar, ScalarType, ScopedScalar, ABSENT_CODE};
use tessera_lifecycle::WalScalar;

use self::arrow::code_at;
pub(crate) use self::arrow::{parse_ingest_batch, parse_values_batch, ParsedBatch, ParsedValues};

/// The two encodings a record-bearing route takes (ingest §1.2). JSON is the default and Arrow
/// IPC is selected by content type; nothing about a route's semantics depends on which carried
/// the batch, since both decode to one row form before the executor sees either.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum BodyEncoding {
    Json,
    Arrow,
}

/// Contracts §1: external IDs are caller-supplied byte strings, capped at **≤ 64 bytes**.
/// Over-length is a typed error here and at build, never a truncation — truncating two callers'
/// keys down to a shared 64-byte prefix would silently merge two different items into one
/// entity, and sidecar disk scales linearly with key length, so the cap is load-bearing, not
/// cosmetic. `/control/ingest` is the only caller-supplied-bytes path in this workspace (the
/// build's external-id representation is fixed at exactly 8 bytes — `tessera-build`'s
/// `BuildError::ExternalIdTooLong` cannot be reached by any build input), so this is where the
/// cap is actually enforced and tested.
pub(crate) const EXTERNAL_ID_MAX_LEN: usize = 64;

/// A group-scoped family as the declaration the row-level helpers take.
///
/// **The declaration is an ordinary attribute's** (`views.md` §5) — same types, same `index` and
/// `render` — and what the scope changes is only which column file a value lands in. So a
/// family's key check, wire type and code minting are the entity-scoped ones, asked of a borrowed
/// declaration built here rather than restated as a second set of rules that could drift from
/// [`DeclaredScalar`]'s.
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

/// A declared scalar as the family-shaped helpers take it — [`scoped_as_declared`]'s inverse,
/// so an omitted declared column takes the same absence an omitted family does.
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

/// What a batch column of this family carries on the wire — [`DeclaredScalar::wire_type`]'s
/// answer, so a scoped category arrives as its **key** exactly as an entity-scoped one does and a
/// caller is never the minting authority for a code (per-point-attributes §3.1, §5).
pub(crate) fn scoped_wire_type(family: &ScopedScalar) -> ScalarType {
    scoped_as_declared(family).wire_type()
}

/// The value a row carries for a family the batch does not mention at all.
///
/// A category spends its reserved code 0, which its vocabulary keeps out of the value space;
/// every other family has no spare bit pattern and travels `Null`, which lands in the column's
/// presence bitmap (decision 0064). The same split [`parse_ingest_batch`] makes per row, restated
/// here for the whole-column case — which a family has and a declared scalar does not, a family
/// having no slot in the positional tail to misalign.
pub(crate) fn scoped_absent(family: &ScopedScalar) -> WalScalar {
    match family.vocabulary {
        Some(_) => code_at(family.arrow_type, ABSENT_CODE),
        None => WalScalar::Null,
    }
}
