//! The attribute filter index: entity-space postings behind the filter contract (§8.2).
//!
//! `docs/design/filter-index.md` is the design. This crate owns the artefact's read side — opening
//! a column's postings and resolving a value to the set of entities carrying it. What a query does
//! with that set is the engine's (`filter-surface.md`).
//!
//! # Why this is a separate crate from `tessera-authz`
//!
//! Both indexes are entity-space, neither mentions row ids, and they share the CSR postings format
//! byte for byte — so a module inside `tessera-authz` would have been cheaper. It is separate
//! because that crate owns `M_auth` and **I3**, while everything here may only ever narrow `M_sel`
//! under **I12**, and `tessera-authz` is the crate an assurer opens first. Keeping its public
//! surface authorisation-only is worth one crate and one dependency edge (filter-index §9).
//!
//! The edge runs this way — filter depends on authz, for the format only — rather than the reverse
//! or through a third crate, because the format's *owner* is the authorisation index: it was built
//! there, its safety argument is discharged there, and a shared crate would move an `unsafe` block's
//! justification away from the code that established it.
//!
//! # What keeps the two indexes apart
//!
//! Three things, and they close different holes:
//!
//! - **Separate files.** An attribute descriptor byte-equal to a satisfied authorisation descriptor
//!   would otherwise union its items into `M_auth`. `DictWriter` interns arbitrary caller-supplied
//!   bytes, so a namespace tag inside a descriptor lives in a space the caller also writes into;
//!   separate files make the collision impossible rather than prevented (per-point-attributes §3.5).
//! - **Separate types.** `AttrLocalId` and `TermId` are both `u32` ordinals read by the same code,
//!   with no conversion between them, pinned by a compile-fail row in `tessera-types`. A file
//!   boundary stops a descriptor collision; only a type boundary stops a call-site one.
//! - **An untyped format core.** `PostingsReader::posting_at` takes a bare `u32` precisely so that
//!   neither newtype has to cross into the other crate. Typing the shared reader in `TermId` would
//!   force this crate to convert at every call — the crossing, reintroduced as boilerplate.
//!
//! # Ordinals are per column, and that is load-bearing
//!
//! An attribute ordinal is local to **one column**: each column owns a postings file and its own
//! ordinal space, so an `AttrTermId` is really the pair `(column, AttrLocalId)`. An earlier design
//! put every column in one positional file addressed as `base + local`; under ingest a value minted
//! after the build takes an ordinal belonging to the next column, the base-union-tiers read then
//! merges one value's members into another's, and because vocabulary visibility is
//! membership-derived that shows a value to a principal on the strength of a different value's
//! members — leak-register row C11, reachable by ordinary operation (filter-index §2.2).

mod column;
mod dict;
mod extent;
mod pack;
mod record;
mod record_stack;
mod values;
mod values_writer;

pub use column::{resolve_union, ColumnPostings};
pub use dict::{
    write_sorted_dict, DictError, DictStats, KeyMatcher, SortedDict, SortedDictWriter,
    DEFAULT_RESTART_INTERVAL, DICT_FILE, DICT_FORMAT_VERSION,
};
pub use extent::{extent_paths, open_extent, write_extent, EXTENTS_DIR};
pub use record::{
    encode_row, RecordBlob, RecordError, RecordField, RecordRowCursor, RecordValue,
    RECORD_BLOCKS_FILE,
    RECORD_BLOCK_TARGET, RECORD_DIRECTORY_FILE, RECORD_HASROW_FILE,
};
pub use record_stack::{RecordExtentPaths, RecordStack};
pub use values::{
    take_scan_work, Access, CodeSet, Codes, Endpoint, Scalar, ScanWork, ValueColumn, PRESENCE_FILE,
    VALUES_FILE,
};
pub use values_writer::{write_value_column, ColumnKind, ValueColumnWriter};
