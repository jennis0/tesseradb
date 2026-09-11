//! The record blob: every blob-resident field's value bytes, one self-describing row per entity,
//! and the fail-closed read that returns them (`records-and-search.md` §3).
//!
//! This module owns the **byte format** — rows, blocks, the directory — and the reader. The block
//! writer lives in `tessera-filter-write`, which is a separate crate for the codegen reason its own
//! module doc gives; keeping encode and decode *here*, in one module, is what keeps the layout a
//! single fact rather than two transcriptions that could drift.
//!
//! # The format
//!
//! **A block** is a header and the rows it holds. The header states what the block is, the rows
//! carry only their fields:
//!
//! ```text
//! block   := row_count u32 LE | first_rank u32 LE | first_entity u32 LE
//!            | extent_digest u64 LE | gap × (row_count - 1) | row × row_count
//! gap     := LEB128 varint: the row's entity less its predecessor's, less one
//! row     := field*
//! field   := tag u16 LE | kind u8 | value
//! kinds   := 0 bool (one byte, 0 or 1 — anything else refuses)
//!            1 u8   2 u16   3 u32   4 u64   5 i8   6 i16   7 i32   8 i64
//!            9 f32  10 f64  11 timestamp_us (i64 LE)
//!            12 utf8 (byte_len u32 LE | bytes, validated UTF-8)
//!            13 list (elem_kind u8 | count u32 LE | count values, element encoding only)
//! ```
//!
//! A row has no header. It begins where the directory's rank-indexed offset places it, relative to
//! the block's rows section, and ends where the next row begins or where the block does; a row's
//! fields must consume that extent exactly. Rows carry no entity id of their own: the block states
//! the first entity once and one varint per row after it, so the identity of every row is in the
//! block that holds it at about a byte apiece rather than four ([decision 0141](../../../docs/decisions/0141-the-record-blob-states-identity-once-per-block.md)).
//!
//! `tag` is the column's position in the manifest's `declared_scalars` — an index internal, like
//! the entity ids beside it, resolved to a column name server-side and never serialised to any
//! client. A field's absence is its absence from the row; there is no null encoding and no
//! per-field presence structure (records §3 — the blob as a whole carries the one has-row bitmap).
//! A duplicate tag within a row refuses at both encode and decode.
//!
//! **The list encoding is specified now and populated in epic 3** (records §5; decision 0013's
//! marking discipline): elements carry the value encoding only — no per-element tag or kind, a
//! `utf8` element keeping its own length prefix — and an element kind of `list` refuses, one level
//! being the whole of the multi model. The decoder implements it so the format is real and pinned
//! by test; [`encode_row`] refuses a list value until the multi surface lands, so no artefact can
//! carry one early.
//!
//! **Blocks**: rows concatenate in ascending entity order and are cut into zstd-compressed blocks
//! against a 256 KiB uncompressed target ([`RECORD_BLOCK_TARGET`]), measured over the rows and not
//! the header. **A row never splits across blocks**: a block seals when appending the next row
//! would pass the target, so a row larger than the target gets an oversized block of its own — the
//! target is a target, not a cap (records §3). Whole-row blocks are what make drill-down one block
//! read and one decompress; the 256 KiB point is the string-storage probe's measured operating
//! point on *title-shaped* bytes (2.44× at 169 µs/read), and the mixed-field row ratio is
//! **assumed, not measured** — records §3 says so and §11 item 6 owes the measurement, so this
//! module quotes neither figure as this format's.
//!
//! # Addressing is has-row rank (records §3, review B5)
//!
//! A **has-row Roaring bitmap** marks the entities that have a row; an entity's rank in it indexes
//! a compacted `u32` array of within-block offsets; a block directory of
//! `(compressed offset, first rank)` locates the block by binary search. An entity with no
//! blob-resident value is absent from the bitmap and occupies nothing anywhere. Both obvious
//! alternatives were wrong: a per-entity offset array pays 4 B for every entity that has no row,
//! and a per-field presence structure re-buys per-column addressing for bytes that share a row.
//!
//! On disk the directory is one Arrow batch, one row per block: the compressed offset **and
//! length**, the uncompressed length, the first rank, and the block's row offsets as a
//! `LargeList<u32>` — large offsets because the flattened child is one element per has-row entity,
//! and the 2³² entity ceiling does not fit an `i32` list. The lengths are derivable from the
//! neighbouring offsets and are carried anyway: redundancy the reader *checks* is what turns an
//! addressing defect into a refusal instead of a wrong answer.
//!
//! # The read is fail-closed (records §3, review B6)
//!
//! The blob's indirection is the one new failure class the attribute artefacts acquire: the other
//! homes are positional, so there is no offset to get wrong, and the digest covers bytes, not
//! addressing consistency. A build or fold defect here would otherwise serve a *neighbour's*
//! record for a visible entity, out of blocks that also hold entities the principal cannot see.
//! Reaching a row is three derived indirections — the has-row rank, the block that holds it, the
//! offset within that block — and each is checked against something the other file states.
//!
//! **The block header is that something.** Its `first_rank` is the directory's claim restated in
//! the bytes the directory addresses, so a corrupt compressed offset or a mis-chosen block refuses
//! instead of answering. Its `first_entity` is the has-row bitmap's rank-`first_rank` member
//! restated in the block, so a corrupt bitmap refuses rather than shifting every row of the blob by
//! one. Its `extent_digest` covers the directory's whole row-offset slice for the block and the
//! rows section's length, so a directory that disagrees with the bytes it addresses refuses before
//! a row is read, whichever of the two files moved. The gaps then give each row its own entity,
//! checked against the entity the caller resolved through the rank, so a wrong rank inside the
//! right block refuses too. Every offset and length is bounds-checked against its block, and any
//! mismatch, short file or malformed directory is a typed [`RecordError`] — never a neighbour's
//! row, never a silent absence.
//!
//! Entity ids in the block are an index internal and are never serialised to any client (**I10**
//! as corrected by decision 0065: the blob is an index internal, not a gather artefact).
//!
//! **What this does not catch**: a block whose *bytes* are corrupt inside a row, in a way that
//! still frames as a field sequence consuming the row's extent exactly. The field walk is what
//! stands there — a kind byte naming no kind, a length running past the extent, a walk that ends
//! short of it all refuse — and the manifest's digest is what stands behind that (records §7: a
//! blob file that fails its digest refuses at open). [`RecordBlob::self_check`] walks the whole
//! artefact — ranks, offsets, block bounds, identities — for the conformance suite and for
//! `tessera verify --deep`, neither of which can see addressing from the served surface
//! (records §10).
//!
//! The has-row bitmap is serialised run-optimised, so the file's bytes are a function of the
//! entity set alone — the same canonicalisation argument the presence bitmap makes in
//! `values_writer.rs`, owed here by every producer (build, flush, coalesce, fold) alike.

use std::fmt;
use std::io;
use std::path::Path;
use std::sync::Arc;

use arrow::array::{Array, LargeListArray, UInt32Array, UInt64Array};
use arrow::buffer::{Buffer, ScalarBuffer};
use croaring::{Bitmap, Portable};

use crate::values::Access;

/// Uncompressed target bytes per block. The string-storage probe's operating point — measured on
/// title-shaped bytes, not on mixed rows (records §3 marks the mixed ratio *assumed*).
pub const RECORD_BLOCK_TARGET: usize = 256 * 1024;

/// The blob's three base files, under `attrs/record/` (records §7). `record` is a reserved column
/// name at schema parse precisely so this namespace cannot collide with a declaration (review N10).
pub const RECORD_BLOCKS_FILE: &str = "blocks.bin";
pub const RECORD_HASROW_FILE: &str = "hasrow.roaring";
pub const RECORD_DIRECTORY_FILE: &str = "directory.arrow";

/// A blob read's refusal, typed so a caller can tell an environment failure from a malformed
/// artefact — the latter is the fail-closed class records §3 defines, and it must never degrade
/// into a silent absence or another entity's row.
#[derive(Debug)]
pub enum RecordError {
    /// The file system failed underneath the read.
    Io(io::Error),
    /// The artefact is malformed: a bounds violation, a discriminant mismatch, a directory that
    /// disagrees with its own blocks, a short file. The request is refused.
    Malformed(String),
}

impl fmt::Display for RecordError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            RecordError::Io(e) => write!(f, "record blob: {e}"),
            RecordError::Malformed(detail) => write!(f, "record blob refuses: {detail}"),
        }
    }
}

impl std::error::Error for RecordError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            RecordError::Io(e) => Some(e),
            RecordError::Malformed(_) => None,
        }
    }
}

impl From<io::Error> for RecordError {
    fn from(e: io::Error) -> Self {
        RecordError::Io(e)
    }
}

impl From<RecordError> for io::Error {
    fn from(e: RecordError) -> Self {
        match e {
            RecordError::Io(inner) => inner,
            RecordError::Malformed(detail) => io::Error::new(io::ErrorKind::InvalidData, detail),
        }
    }
}

fn malformed(detail: impl Into<String>) -> RecordError {
    RecordError::Malformed(detail.into())
}

/// One field's typed value, as a row carries it. Mirrors the declarable scalar set
/// (contracts §2.2) plus the list a `multi` field becomes in epic 3; absence is not a value —
/// an absent field is absent from the row.
#[derive(Debug, Clone, PartialEq)]
pub enum RecordValue {
    Bool(bool),
    U8(u8),
    U16(u16),
    U32(u32),
    U64(u64),
    I8(i8),
    I16(i16),
    I32(i32),
    I64(i64),
    F32(f32),
    F64(f64),
    /// Microseconds since the Unix epoch — its own kind rather than an `i64`, so the row states
    /// the unit the declaration states and the two can be cross-checked.
    TimestampUs(i64),
    Utf8(String),
    /// ⊘ Specified, produced by no writer until epic 3's multi surface (records §5). Homogeneous
    /// scalar elements, one level deep; [`encode_row`] refuses it.
    List(Vec<RecordValue>),
}

/// One field of a row: the declared column's position, and its value.
#[derive(Debug, Clone, PartialEq)]
pub struct RecordField {
    /// The column's position in the manifest's `declared_scalars` — an index internal, never
    /// serialised to any client.
    pub tag: u16,
    pub value: RecordValue,
}

const KIND_BOOL: u8 = 0;
const KIND_U8: u8 = 1;
const KIND_U16: u8 = 2;
const KIND_U32: u8 = 3;
const KIND_U64: u8 = 4;
const KIND_I8: u8 = 5;
const KIND_I16: u8 = 6;
const KIND_I32: u8 = 7;
const KIND_I64: u8 = 8;
const KIND_F32: u8 = 9;
const KIND_F64: u8 = 10;
const KIND_TIMESTAMP_US: u8 = 11;
const KIND_UTF8: u8 = 12;
const KIND_LIST: u8 = 13;

/// A block's fixed header: the row count, the first rank and the first entity, four bytes each,
/// then the eight-byte extent digest. The per-row entity gaps follow it, and the rows follow
/// those.
const BLOCK_HEADER_FIXED: usize = 20;

/// The digest a block carries of the extents the directory addresses it by: FNV-1a over 64 bits,
/// folded over every row's start within the block's rows section, in order, then over that
/// section's length.
///
/// The writer takes it over the offsets it is about to hand the directory; the reader takes it
/// over the offsets the directory holds. The two files therefore disagree loudly rather than
/// addressing past each other, which neither file can state on its own.
///
/// An addressing check, not an integrity one: it says the directory's offsets are the ones
/// written beside these bytes, and says nothing about a file that was rewritten wholesale. The
/// manifest's SHA-256 answers that question (records §7).
pub fn extent_digest(offsets: &[u32], rows_len: u32) -> u64 {
    let mut hash: u64 = 0xcbf2_9ce4_8422_2325;
    let mut fold = |value: u32| {
        for byte in value.to_le_bytes() {
            hash ^= u64::from(byte);
            hash = hash.wrapping_mul(0x0000_0100_0000_01b3);
        }
    };
    for offset in offsets {
        fold(*offset);
    }
    fold(rows_len);
    hash
}

fn put_varint(mut value: u32, out: &mut Vec<u8>) {
    loop {
        let byte = (value & 0x7f) as u8;
        value >>= 7;
        if value == 0 {
            out.push(byte);
            return;
        }
        out.push(byte | 0x80);
    }
}

/// One LEB128 varint out of `bytes`, refusing a truncated or over-wide one. The caller passes the
/// bound the varint may not cross: the whole block while the gap section's own end is still being
/// found, the end of that section once it is known.
fn take_varint(bytes: &[u8], cursor: &mut usize) -> Result<u32, RecordError> {
    let mut value: u32 = 0;
    let mut shift = 0u32;
    loop {
        let Some(byte) = bytes.get(*cursor).copied() else {
            return Err(malformed(
                "a block's entity gaps end in the middle of a varint",
            ));
        };
        *cursor += 1;
        let payload = u32::from(byte & 0x7f);
        let Some(shifted) = payload.checked_shl(shift).filter(|v| (v >> shift) == payload) else {
            return Err(malformed("a block's entity gap does not fit u32"));
        };
        value |= shifted;
        if byte & 0x80 == 0 {
            if shift > 0 && byte == 0 {
                return Err(malformed(
                    "a block's entity gap is padded with a redundant continuation byte",
                ));
            }
            return Ok(value);
        }
        shift += 7;
        if shift >= 32 {
            return Err(malformed("a block's entity gap does not fit u32"));
        }
    }
}

/// Write one block's header: what the directory claims about the block, restated in the block,
/// and every row's entity as a gap from its predecessor.
///
/// Refuses an empty block and a non-ascending entity list. The gap stored is the distance less
/// one, so strict ascent is not a rule the decoder checks but a shape it cannot express: every
/// decoded gap adds at least one.
pub fn encode_block_header(
    first_rank: u32,
    entities: &[u32],
    extent_digest: u64,
    out: &mut Vec<u8>,
) -> Result<(), RecordError> {
    let Some((&first, rest)) = entities.split_first() else {
        return Err(malformed(
            "a block with no rows is never written (records §3)",
        ));
    };
    let Ok(row_count) = u32::try_from(entities.len()) else {
        return Err(malformed(
            "more rows in one block than the u32 rank space holds",
        ));
    };
    out.extend_from_slice(&row_count.to_le_bytes());
    out.extend_from_slice(&first_rank.to_le_bytes());
    out.extend_from_slice(&first.to_le_bytes());
    out.extend_from_slice(&extent_digest.to_le_bytes());
    let mut previous = first;
    for &entity in rest {
        let Some(gap) = entity.checked_sub(previous).filter(|g| *g > 0) else {
            return Err(malformed(format!(
                "entity {entity} arrived at or below its predecessor {previous} in one block; \
                 rows are in strictly ascending entity order (I9)"
            )));
        };
        put_varint(gap - 1, out);
        previous = entity;
    }
    Ok(())
}

/// A block's header, decoded, with where its two variable sections begin.
#[derive(Debug, Clone, Copy)]
struct BlockHeader {
    row_count: usize,
    first_rank: u32,
    first_entity: u32,
    extent_digest: u64,
    /// Where the entity gaps begin in the uncompressed block.
    gaps_at: usize,
    /// Where the rows begin, one past the last gap.
    rows_at: usize,
}

fn decode_block_header(bytes: &[u8], block: usize) -> Result<BlockHeader, RecordError> {
    if bytes.len() < BLOCK_HEADER_FIXED {
        return Err(malformed(format!(
            "block {block} decompressed to {} bytes, too few for a block header",
            bytes.len()
        )));
    }
    let word = |at: usize| u32::from_le_bytes(bytes[at..at + 4].try_into().expect("four bytes"));
    let row_count = word(0) as usize;
    if row_count == 0 {
        return Err(malformed(format!(
            "block {block} states no rows; an empty block is never written"
        )));
    }
    let first_rank = word(4);
    let first_entity = word(8);
    let extent_digest = u64::from_le_bytes(bytes[12..20].try_into().expect("eight bytes"));
    let gaps_at = BLOCK_HEADER_FIXED;
    let mut cursor = gaps_at;
    for _ in 1..row_count {
        take_varint(bytes, &mut cursor)?;
    }
    Ok(BlockHeader {
        row_count,
        first_rank,
        first_entity,
        extent_digest,
        gaps_at,
        rows_at: cursor,
    })
}

impl BlockHeader {
    /// The entity of the row at `local` within the block, walked from the first entity through
    /// the gaps. The walk is what makes the identity the *block's* statement rather than the
    /// bitmap's, which is the whole of the wrong-rank check.
    fn entity_at(&self, bytes: &[u8], local: usize) -> Result<u32, RecordError> {
        let mut entity = self.first_entity;
        let mut cursor = self.gaps_at;
        let gaps = &bytes[..self.rows_at];
        for _ in 0..local {
            let gap = take_varint(gaps, &mut cursor)?;
            entity = entity
                .checked_add(gap)
                .and_then(|e| e.checked_add(1))
                .ok_or_else(|| malformed("a block's entity gaps run past the entity ceiling"))?;
        }
        Ok(entity)
    }
}


/// Append one entity's row to `out` — the writer's half of the format, kept beside the decoder so
/// the layout exists in one module. A row is its fields and nothing else; the block header states
/// whose row it is and the directory states where it starts.
///
/// Refuses: an empty field list (a row with no fields is an absence, and absence is absence from
/// the has-row bitmap — see records §3); a duplicate tag; and a list value (specified for epic 3,
/// produced by nothing until the multi surface lands — records §5). `entity` names the row in
/// those refusals and is not encoded.
pub fn encode_row(
    entity: u32,
    fields: &[RecordField],
    out: &mut Vec<u8>,
) -> Result<(), RecordError> {
    if fields.is_empty() {
        return Err(malformed(format!(
            "entity {entity} was given a row with no fields; an entity with no blob-resident \
             value has no row at all (records §3)"
        )));
    }
    for (i, field) in fields.iter().enumerate() {
        if fields[..i].iter().any(|f| f.tag == field.tag) {
            return Err(malformed(format!(
                "entity {entity} carries field tag {} twice; a field's value is one value",
                field.tag
            )));
        }
    }
    for field in fields {
        out.extend_from_slice(&field.tag.to_le_bytes());
        encode_value(entity, &field.value, out)?;
    }
    Ok(())
}

fn encode_value(entity: u32, value: &RecordValue, out: &mut Vec<u8>) -> Result<(), RecordError> {
    match value {
        RecordValue::Bool(b) => out.extend_from_slice(&[KIND_BOOL, u8::from(*b)]),
        RecordValue::U8(v) => out.extend_from_slice(&[KIND_U8, *v]),
        RecordValue::U16(v) => {
            out.push(KIND_U16);
            out.extend_from_slice(&v.to_le_bytes());
        }
        RecordValue::U32(v) => {
            out.push(KIND_U32);
            out.extend_from_slice(&v.to_le_bytes());
        }
        RecordValue::U64(v) => {
            out.push(KIND_U64);
            out.extend_from_slice(&v.to_le_bytes());
        }
        RecordValue::I8(v) => {
            out.push(KIND_I8);
            out.extend_from_slice(&v.to_le_bytes());
        }
        RecordValue::I16(v) => {
            out.push(KIND_I16);
            out.extend_from_slice(&v.to_le_bytes());
        }
        RecordValue::I32(v) => {
            out.push(KIND_I32);
            out.extend_from_slice(&v.to_le_bytes());
        }
        RecordValue::I64(v) => {
            out.push(KIND_I64);
            out.extend_from_slice(&v.to_le_bytes());
        }
        RecordValue::F32(v) => {
            out.push(KIND_F32);
            out.extend_from_slice(&v.to_le_bytes());
        }
        RecordValue::F64(v) => {
            out.push(KIND_F64);
            out.extend_from_slice(&v.to_le_bytes());
        }
        RecordValue::TimestampUs(v) => {
            out.push(KIND_TIMESTAMP_US);
            out.extend_from_slice(&v.to_le_bytes());
        }
        RecordValue::Utf8(s) => {
            let Ok(len) = u32::try_from(s.len()) else {
                return Err(malformed(format!(
                    "entity {entity} carries a string past u32::MAX bytes"
                )));
            };
            out.push(KIND_UTF8);
            out.extend_from_slice(&len.to_le_bytes());
            out.extend_from_slice(s.as_bytes());
        }
        RecordValue::List(_) => {
            return Err(malformed(format!(
                "entity {entity} carries a list value; the list encoding is specified but \
                 populated only when the multi surface lands (records §5), so writing one is \
                 refused"
            )));
        }
    }
    Ok(())
}

/// A bounds-checked little-endian read out of a row's payload.
macro_rules! take {
    ($payload:expr, $cursor:expr, $n:expr, $what:expr) => {{
        let lo = *$cursor;
        let hi = lo.checked_add($n).filter(|hi| *hi <= $payload.len());
        let Some(hi) = hi else {
            return Err(malformed(format!(
                "a row's {} runs past the end of its payload",
                $what
            )));
        };
        *$cursor = hi;
        &$payload[lo..hi]
    }};
}

fn decode_value(
    payload: &[u8],
    cursor: &mut usize,
    kind: u8,
    lists_allowed: bool,
) -> Result<RecordValue, RecordError> {
    macro_rules! fixed {
        ($variant:ident, $t:ty, $what:expr) => {{
            let bytes = take!(payload, cursor, std::mem::size_of::<$t>(), $what);
            RecordValue::$variant(<$t>::from_le_bytes(
                bytes.try_into().expect("take! returned the exact width"),
            ))
        }};
    }
    Ok(match kind {
        KIND_BOOL => {
            let byte = take!(payload, cursor, 1, "bool value")[0];
            match byte {
                0 => RecordValue::Bool(false),
                1 => RecordValue::Bool(true),
                other => {
                    return Err(malformed(format!(
                        "a bool field holds byte {other}; only 0 and 1 are bools"
                    )))
                }
            }
        }
        KIND_U8 => RecordValue::U8(take!(payload, cursor, 1, "u8 value")[0]),
        KIND_U16 => fixed!(U16, u16, "u16 value"),
        KIND_U32 => fixed!(U32, u32, "u32 value"),
        KIND_U64 => fixed!(U64, u64, "u64 value"),
        KIND_I8 => RecordValue::I8(take!(payload, cursor, 1, "i8 value")[0] as i8),
        KIND_I16 => fixed!(I16, i16, "i16 value"),
        KIND_I32 => fixed!(I32, i32, "i32 value"),
        KIND_I64 => fixed!(I64, i64, "i64 value"),
        KIND_F32 => fixed!(F32, f32, "f32 value"),
        KIND_F64 => fixed!(F64, f64, "f64 value"),
        KIND_TIMESTAMP_US => fixed!(TimestampUs, i64, "timestamp value"),
        KIND_UTF8 => {
            let len = take!(payload, cursor, 4, "string length");
            let len = u32::from_le_bytes(len.try_into().expect("four bytes")) as usize;
            let bytes = take!(payload, cursor, len, "string bytes");
            let text = std::str::from_utf8(bytes)
                .map_err(|_| malformed("a utf8 field holds bytes that are not UTF-8"))?;
            RecordValue::Utf8(text.to_string())
        }
        KIND_LIST => {
            if !lists_allowed {
                return Err(malformed(
                    "a list element is itself a list; the multi model is one level deep \
                     (records §5)",
                ));
            }
            let elem_kind = take!(payload, cursor, 1, "list element kind")[0];
            let count = take!(payload, cursor, 4, "list length");
            let count = u32::from_le_bytes(count.try_into().expect("four bytes")) as usize;
            let mut elements = Vec::with_capacity(count.min(payload.len()));
            for _ in 0..count {
                elements.push(decode_value(payload, cursor, elem_kind, false)?);
            }
            RecordValue::List(elements)
        }
        other => {
            return Err(malformed(format!(
                "a field carries kind byte {other}, which names no value kind"
            )))
        }
    })
}

/// Decode one row's fields out of the extent the directory places it at.
///
/// The extent is the whole of the row: the field walk must consume it exactly. A walk that runs
/// past the extent refuses on bounds, and one that ends short of it cannot — the loop runs until
/// the cursor reaches the end — so a directory offset that lands mid-row refuses unless the bytes
/// there happen to frame as a whole field sequence. `entity` names the row in the refusals; it is
/// checked against the block's own statement of identity before this is called.
fn decode_row(payload: &[u8], entity: u32) -> Result<Vec<RecordField>, RecordError> {
    let mut fields = Vec::new();
    let mut cursor = 0usize;
    while cursor < payload.len() {
        let tag = take!(payload, &mut cursor, 2, "field tag");
        let tag = u16::from_le_bytes(tag.try_into().expect("two bytes"));
        if fields.iter().any(|f: &RecordField| f.tag == tag) {
            return Err(malformed(format!(
                "entity {entity}'s row carries field tag {tag} twice"
            )));
        }
        let kind = take!(payload, &mut cursor, 1, "field kind")[0];
        let value = decode_value(payload, &mut cursor, kind, true)?;
        fields.push(RecordField { tag, value });
    }
    if fields.is_empty() {
        return Err(malformed(format!(
            "entity {entity}'s row carries no fields; an entity with no blob-resident value has \
             no row at all"
        )));
    }
    Ok(fields)
}

/// The record blob, opened for reading: the has-row bitmap, the block directory, and the block
/// bytes (mapped under [`Access::Mapped`], the request path's mode).
///
/// Everything the directory claims about itself is verified at open — blocks contiguous and
/// exactly covering `blocks.bin`, first ranks agreeing with the per-block row counts, the total
/// agreeing with the has-row cardinality — so a truncated or doctored artefact refuses before any
/// request reads through it. What open cannot see (row offsets against uncompressed bytes, the
/// discriminants) is checked per read, and exhaustively by [`Self::self_check`].
#[derive(Debug)]
pub struct RecordBlob {
    hasrow: Bitmap,
    blocks: Buffer,
    compressed_offset: ScalarBuffer<u64>,
    compressed_len: ScalarBuffer<u64>,
    uncompressed_len: ScalarBuffer<u32>,
    first_rank: ScalarBuffer<u32>,
    /// Per-block boundaries into `row_offsets` — the list offsets of the directory's
    /// `row_offsets` column, `block_count + 1` entries.
    list_offsets: ScalarBuffer<i64>,
    /// The compacted rank-indexed within-block offsets, one per has-row entity (records §3).
    row_offsets: ScalarBuffer<u32>,
}

impl RecordBlob {
    /// Open the base blob under `dir` — `attrs/record/` — by its three canonical file names.
    pub fn open_dir(dir: &Path, access: Access) -> Result<Self, RecordError> {
        Self::open(
            &dir.join(RECORD_BLOCKS_FILE),
            &dir.join(RECORD_HASROW_FILE),
            &dir.join(RECORD_DIRECTORY_FILE),
            access,
        )
    }

    /// Open a blob from explicit paths (an extent's files carry flush-scoped names).
    ///
    /// A missing, short or malformed file refuses here. The manifest's digest check is the outer
    /// guard (records §7: a blob file that is missing, short, or fails its digest refuses at
    /// open); these checks are the addressing's own, which a digest cannot express.
    pub fn open(
        blocks_path: &Path,
        hasrow_path: &Path,
        directory_path: &Path,
        access: Access,
    ) -> Result<Self, RecordError> {
        let hasrow_bytes = std::fs::read(hasrow_path)?;
        let hasrow = Bitmap::try_deserialize::<Portable>(&hasrow_bytes).ok_or_else(|| {
            malformed(format!(
                "the has-row bitmap at {} is not portable Roaring",
                hasrow_path.display()
            ))
        })?;
        let blocks = read_buffer(blocks_path, access)?;
        let directory = read_buffer(directory_path, access)?;
        let batch = tessera_authz::decode_single_batch(
            &directory,
            &format!("{}", directory_path.display()),
        )?;
        if batch.num_columns() != 5 {
            return Err(malformed(format!(
                "the block directory at {} has {} columns where the format has 5",
                directory_path.display(),
                batch.num_columns()
            )));
        }
        let dir_column = |i: usize| -> Result<&UInt64Array, RecordError> {
            batch.column(i).as_any().downcast_ref().ok_or_else(|| {
                malformed(format!(
                    "directory column {i} is not the u64 the format states"
                ))
            })
        };
        let dir_column_u32 = |i: usize| -> Result<&UInt32Array, RecordError> {
            batch.column(i).as_any().downcast_ref().ok_or_else(|| {
                malformed(format!(
                    "directory column {i} is not the u32 the format states"
                ))
            })
        };
        let compressed_offset = dir_column(0)?.values().clone();
        let compressed_len = dir_column(1)?.values().clone();
        let uncompressed_len = dir_column_u32(2)?.values().clone();
        let first_rank = dir_column_u32(3)?.values().clone();
        let lists: &LargeListArray = batch.column(4).as_any().downcast_ref().ok_or_else(|| {
            malformed("directory column 4 is not the LargeList<u32> the format states")
        })?;
        let offsets_child: &UInt32Array =
            lists.values().as_any().downcast_ref().ok_or_else(|| {
                malformed("the directory's row offsets are not the u32 the format states")
            })?;
        if (0..batch.num_columns()).any(|i| batch.column(i).null_count() != 0)
            || offsets_child.null_count() != 0
        {
            return Err(malformed(
                "the block directory carries nulls; every directory value is required",
            ));
        }
        let list_offsets = lists.offsets().clone().into_inner();
        let row_offsets = offsets_child.values().clone();

        let blob = RecordBlob {
            hasrow,
            blocks,
            compressed_offset,
            compressed_len,
            uncompressed_len,
            first_rank,
            list_offsets,
            row_offsets,
        };
        blob.check_directory()?;
        Ok(blob)
    }

    /// The open-time half of the fail-closed rule: the directory must agree with itself, with
    /// `blocks.bin`'s actual length, and with the has-row bitmap, before anything reads through
    /// it. Each redundant quantity the format carries is compared here, which is what the
    /// redundancy is for.
    fn check_directory(&self) -> Result<(), RecordError> {
        let blocks = self.block_count();
        if self.list_offsets.len() != blocks + 1 {
            return Err(malformed(format!(
                "the directory's row-offset lists disagree with its {blocks} blocks"
            )));
        }
        let mut at: u64 = 0;
        let mut rank: u64 = 0;
        for block in 0..blocks {
            if self.compressed_offset[block] != at {
                return Err(malformed(format!(
                    "block {block} claims compressed offset {} where the preceding blocks end at \
                     {at}; blocks are contiguous by construction, so the directory is inconsistent",
                    self.compressed_offset[block]
                )));
            }
            at = at
                .checked_add(self.compressed_len[block])
                .ok_or_else(|| malformed("the directory's compressed lengths overflow u64"))?;
            if self.first_rank[block] as u64 != rank {
                return Err(malformed(format!(
                    "block {block} claims first rank {} where the preceding blocks account for \
                     {rank} rows",
                    self.first_rank[block]
                )));
            }
            let rows = self.rows_in_block(block)?;
            if rows == 0 {
                return Err(malformed(format!(
                    "block {block} holds no rows; an empty block is never written"
                )));
            }
            rank += rows as u64;
        }
        if at != self.blocks.len() as u64 {
            return Err(malformed(format!(
                "the directory accounts for {at} compressed bytes but blocks.bin holds {}; the \
                 file is short or carries bytes no block claims",
                self.blocks.len()
            )));
        }
        if rank != self.hasrow.cardinality() {
            return Err(malformed(format!(
                "the directory addresses {rank} rows but the has-row bitmap holds {} entities",
                self.hasrow.cardinality()
            )));
        }
        Ok(())
    }

    /// How many blocks the blob holds. Zero for a blob whose schema has blob-resident columns but
    /// whose corpus supplied no value for any of them.
    pub fn block_count(&self) -> usize {
        self.compressed_offset.len()
    }

    /// How many entities have a row.
    pub fn rows(&self) -> u64 {
        self.hasrow.cardinality()
    }

    /// Whether `entity` has a blob row. Absence is an answer, not an error: an entity all of
    /// whose declared fields live in the other two homes, or which carries no blob-resident
    /// value, legitimately has none.
    pub fn has_row(&self, entity: u32) -> bool {
        self.hasrow.contains(entity)
    }

    fn rows_in_block(&self, block: usize) -> Result<usize, RecordError> {
        let lo = self.list_offsets[block];
        let hi = self.list_offsets[block + 1];
        if lo < 0 || hi < lo || hi as usize > self.row_offsets.len() {
            return Err(malformed(format!(
                "block {block}'s row-offset list bounds ({lo}..{hi}) are malformed"
            )));
        }
        Ok((hi - lo) as usize)
    }

    /// One block, decompressed, its length checked against the directory's claim. The compressed
    /// range was bounds-checked against the file at open.
    fn block_bytes(&self, block: usize) -> Result<Vec<u8>, RecordError> {
        let offset = self.compressed_offset[block] as usize;
        let len = self.compressed_len[block] as usize;
        let expect = self.uncompressed_len[block] as usize;
        let bytes =
            zstd::bulk::decompress(&self.blocks[offset..offset + len], expect).map_err(|e| {
                malformed(format!(
                    "block {block} does not decompress: {e}; the block is corrupt and every row \
                     in it is unaddressable"
                ))
            })?;
        if bytes.len() != expect {
            return Err(malformed(format!(
                "block {block} decompressed to {} bytes where the directory claims {expect}",
                bytes.len()
            )));
        }
        Ok(bytes)
    }

    /// The block holding `rank`: the last one whose first rank is at or below it — always
    /// defined, since `check_directory` proved block 0's first rank is 0.
    fn block_of(&self, rank: u32) -> usize {
        self.first_rank.partition_point(|&fr| fr <= rank) - 1
    }

    /// A decompressed block's header, checked against everything the other two files say about
    /// the block: the directory's row count and first rank, the has-row bitmap's member at that
    /// rank, and the directory's own row-offset slice through the extent digest. Past this, the
    /// block and the files that address it are known to be describing the same rows.
    fn header_of(&self, block: usize, bytes: &[u8]) -> Result<BlockHeader, RecordError> {
        let header = decode_block_header(bytes, block)?;
        let rows = self.rows_in_block(block)?;
        // The extent digest below covers the count as well, so this comparison detects nothing
        // that one would miss. What it does is bound the row index `extent_of` takes from the
        // header against the offsets the directory actually holds, and name the disagreement.
        if header.row_count != rows {
            return Err(malformed(format!(
                "block {block} states {} rows where the directory addresses {rows}",
                header.row_count
            )));
        }
        if header.first_rank != self.first_rank[block] {
            return Err(malformed(format!(
                "block {block} states first rank {} where the directory places it at {}; the \
                 block and the directory address different rows",
                header.first_rank, self.first_rank[block]
            )));
        }
        let expect = self.hasrow.select(header.first_rank).ok_or_else(|| {
            malformed(format!(
                "block {block} claims first rank {} which the has-row bitmap has no member for",
                header.first_rank
            ))
        })?;
        if header.first_entity != expect {
            return Err(malformed(format!(
                "block {block} states first entity {} where the has-row bitmap's rank-{} member \
                 is {expect}; the bitmap and the blocks disagree about which entity a rank names",
                header.first_entity, header.first_rank
            )));
        }
        let list_lo = self.list_offsets[block] as usize;
        let offsets = &self.row_offsets[list_lo..list_lo + rows];
        let rows_len = bytes.len() - header.rows_at;
        let Ok(rows_len) = u32::try_from(rows_len) else {
            return Err(malformed(format!(
                "block {block}'s rows exceed u32::MAX bytes"
            )));
        };
        if extent_digest(offsets, rows_len) != header.extent_digest {
            return Err(malformed(format!(
                "block {block}'s extent digest does not match the directory's row offsets for it; \
                 the directory and the block disagree about where this block's rows begin and the \
                 read is refused rather than answered out of the wrong bytes"
            )));
        }
        Ok(header)
    }

    /// Where the row at `local` starts and ends within a block's rows section, bounds-checked.
    /// The offsets were proved to be the ones written beside these bytes by the extent digest;
    /// this is the arithmetic that turns two of them into a slice.
    fn extent_of(
        &self,
        block: usize,
        header: &BlockHeader,
        block_len: usize,
        local: usize,
    ) -> Result<(usize, usize), RecordError> {
        let list_lo = self.list_offsets[block] as usize;
        let start = self.row_offsets[list_lo + local] as usize;
        let end = if local + 1 < header.row_count {
            self.row_offsets[list_lo + local + 1] as usize
        } else {
            block_len - header.rows_at
        };
        if start >= end || header.rows_at + end > block_len {
            return Err(malformed(format!(
                "block {block} row {local} is placed at [{start}, {end}) in a {}-byte rows \
                 section; the directory does not address this block's bytes",
                block_len - header.rows_at
            )));
        }
        Ok((header.rows_at + start, header.rows_at + end))
    }

    /// One row out of an already-decompressed block, addressed by its rank.
    ///
    /// **The per-read half of the fail-closed rule, stated once.** Both readers that address a
    /// single row — [`Self::fields_of`] and [`Self::for_each_row_in`] — come through here, so the
    /// identity check cannot hold on one route and not the other.
    fn row_at(
        &self,
        bytes: &[u8],
        block: usize,
        header: &BlockHeader,
        rank: u32,
        entity: u32,
    ) -> Result<Vec<RecordField>, RecordError> {
        let Some(local) = rank.checked_sub(header.first_rank).map(|l| l as usize) else {
            return Err(malformed(format!(
                "rank {rank} was addressed into block {block}, whose rows begin at rank {}",
                header.first_rank
            )));
        };
        if local >= header.row_count {
            return Err(malformed(format!(
                "rank {rank} was addressed into block {block}, which holds {} rows from rank {}",
                header.row_count, header.first_rank
            )));
        }
        let carried = header.entity_at(bytes, local)?;
        if carried != entity {
            return Err(malformed(format!(
                "the row addressed for entity {entity} is block {block}'s row {local}, which the \
                 block says belongs to entity {carried}; the addressing is inconsistent and the \
                 request is refused rather than answered with another entity's record \
                 (records §3, review B6)"
            )));
        }
        let (start, end) = self.extent_of(block, header, bytes.len(), local)?;
        decode_row(&bytes[start..end], entity)
    }

    /// The whole row `entity` carries, decoded — or `None` where it has none.
    ///
    /// One block read and one decompress (records §3's cost shape). Every failure of the
    /// addressing — an offset outside the block, a row that does not tile against its neighbour,
    /// a discriminant naming another entity — refuses rather than answers.
    ///
    /// **One entity per call, one decompress per call**, with nothing held between calls: a
    /// caller that wants many rows must not loop this, and [`Self::for_each_row_in`] is the read
    /// for that. Measured on the GeoNames bundle, a served artifact's name cost ~163 µs through
    /// here — 408 ms for the 2 518 artifacts of one viewport, against 1.3 ms for the same viewport
    /// with no layer.
    pub fn fields_of(&self, entity: u32) -> Result<Option<Vec<RecordField>>, RecordError> {
        if !self.hasrow.contains(entity) {
            return Ok(None);
        }
        let rank = (self.hasrow.rank(entity) - 1) as u32;
        let block = self.block_of(rank);
        let bytes = self.block_bytes(block)?;
        let header = self.header_of(block, &bytes)?;
        self.row_at(&bytes, block, &header, rank, entity).map(Some)
    }

    /// The rows of the entities in `wanted` that this blob holds, **decompressing each block it
    /// touches once**, in ascending entity order.
    ///
    /// This is [`Self::fields_of`] amortised over a set, and the set is what makes it worth
    /// having: entities ascend, so ranks ascend, so blocks ascend, and one `Vec<u8>` of block
    /// bytes serves every wanted row inside it. Cost is O(blocks touched) rather than
    /// O(entities), which for a set contiguous in entity space — an artifact level, whose ids are
    /// one reserved run — is the difference between a decompress per artifact and one per 256 KiB
    /// of rows.
    ///
    /// An entity in `wanted` with no row is simply not visited; absence is an answer here exactly
    /// as it is for [`Self::fields_of`], and never an error.
    pub fn for_each_row_in(
        &self,
        wanted: &Bitmap,
        f: &mut dyn FnMut(u32, Vec<RecordField>) -> Result<(), RecordError>,
    ) -> Result<(), RecordError> {
        let mut present = wanted.clone();
        present.and_inplace(&self.hasrow);
        let mut loaded: Option<(usize, BlockHeader)> = None;
        let mut bytes: Vec<u8> = Vec::new();
        for entity in present.iter() {
            let rank = (self.hasrow.rank(entity) - 1) as u32;
            let block = self.block_of(rank);
            if loaded.map(|(b, _)| b) != Some(block) {
                bytes = self.block_bytes(block)?;
                loaded = Some((block, self.header_of(block, &bytes)?));
            }
            let header = loaded.expect("just loaded").1;
            f(entity, self.row_at(&bytes, block, &header, rank, entity)?)?;
        }
        Ok(())
    }

    /// Every row in has-row (= entity) order, decompressing each block once — the read the
    /// lifecycle's two producers stream a whole layer through (records §7: the coalesce's
    /// repacking concatenation and the fold's blanking rewrite both re-emit what this yields).
    ///
    /// **The walk is the self-check**, deliberately: everything [`Self::self_check`] verifies —
    /// blocks decompressing to their claimed lengths, rows tiling each block exactly, the
    /// discriminants agreeing rank-for-rank with the has-row bitmap — is verified here as the rows
    /// stream, and `self_check` *is* this walk with an empty visitor. A producer that re-emitted
    /// rows through a laxer path would launder an addressing defect into a clean-looking output
    /// artefact, which is exactly the class records §3 makes a refusal.
    pub fn for_each_row(
        &self,
        f: &mut dyn FnMut(u32, Vec<RecordField>) -> Result<(), RecordError>,
    ) -> Result<(), RecordError> {
        let mut rows = self.rows_cursor();
        while let Some((entity, fields)) = rows.next_row()? {
            f(entity, fields)?;
        }
        Ok(())
    }

    /// The same walk as a cursor the caller pulls from, which is what a merge over several blobs
    /// needs: a merge takes the lowest head across its inputs and cannot be driven by a visitor.
    pub fn rows_cursor(&self) -> RecordRowCursor<'_> {
        RecordRowCursor::over(self, 0, self.block_count(), true)
    }

    /// A cursor over the rows of blocks `[lo, hi)`, for a pass that divides one blob across
    /// workers. The rows of a block range are a contiguous ascending run of entities, so a range
    /// is a window in entity space as well as in bytes.
    ///
    /// The whole-blob checks a full walk makes do not all hold of a range: the has-row bitmap
    /// carries entities this range does not address, and the range's rows do not start at rank 0.
    /// Everything a block can be checked for on its own — its rows tiling it exactly, each row
    /// decoding inside it, the discriminant agreeing with the has-row rank — is checked here as
    /// it is there.
    pub fn rows_cursor_over(&self, lo: usize, hi: usize) -> RecordRowCursor<'_> {
        RecordRowCursor::over(self, lo, hi.min(self.block_count()), false)
    }

    /// The has-row bitmap — the entity set this layer holds a row for. Borrowed by the lifecycle
    /// producers, whose merge-order and duplicate refusals are set operations over the layers'
    /// bitmaps before any row is read.
    pub fn hasrow(&self) -> &Bitmap {
        &self.hasrow
    }

    /// The addressing self-consistency check the conformance suite calls (records §3, §10): every
    /// block decompresses to its claimed length, every row decodes inside its bounds, rows tile
    /// each block exactly, and the discriminants agree rank-for-rank with the has-row bitmap.
    /// The fixture-input relation cannot see any of this — it is the one artefact-level check the
    /// blob adds (review B7).
    pub fn self_check(&self) -> Result<(), RecordError> {
        self.for_each_row(&mut |_, _| Ok(()))
    }
}

/// A pull walk over a blob's rows in entity order, decompressing one block at a time.
///
/// [`RecordBlob::for_each_row`] is this cursor drained, so there is one walk over a blob and one
/// place its addressing is verified. A merge over several blobs takes the lowest head across its
/// inputs, which a visitor cannot express.
pub struct RecordRowCursor<'a> {
    blob: &'a RecordBlob,
    /// The has-row bitmap positioned at the next row's rank.
    entities: croaring::bitmap::BitmapCursor<'a>,
    /// The block being read, and one past the last this cursor covers.
    block: usize,
    end_block: usize,
    /// The open block's bytes and its checked header.
    bytes: Vec<u8>,
    header: Option<BlockHeader>,
    /// The next row's index within the open block, the entity the block says it belongs to, and
    /// where the gap walk has reached.
    local: usize,
    entity: u32,
    gaps_at: usize,
    /// The byte the previous row ended at, in the block's rows section.
    tiled_to: usize,
    /// Whether the has-row bitmap must be exhausted when the last block is done, which holds of a
    /// walk over the whole blob and not of one over a block range.
    whole: bool,
}

impl<'a> RecordRowCursor<'a> {
    fn over(blob: &'a RecordBlob, lo: usize, hi: usize, whole: bool) -> Self {
        let mut entities = blob.hasrow.cursor();
        if lo < blob.block_count() {
            entities.skip(blob.first_rank[lo]);
        }
        RecordRowCursor {
            blob,
            entities,
            block: lo,
            end_block: hi,
            bytes: Vec::new(),
            header: None,
            local: 0,
            entity: 0,
            gaps_at: 0,
            tiled_to: 0,
            whole,
        }
    }

    /// The next row, or `None` at the end of the cursor's blocks.
    #[allow(clippy::should_implement_trait)]
    pub fn next_row(&mut self) -> Result<Option<(u32, Vec<RecordField>)>, RecordError> {
        loop {
            if self.block >= self.end_block {
                if self.whole && self.entities.has_value() {
                    return Err(malformed(
                        "the has-row bitmap holds entities the directory never addresses",
                    ));
                }
                return Ok(None);
            }
            let header = match self.header {
                Some(header) => header,
                None => {
                    self.bytes = self.blob.block_bytes(self.block)?;
                    let header = self.blob.header_of(self.block, &self.bytes)?;
                    self.header = Some(header);
                    self.local = 0;
                    self.entity = header.first_entity;
                    self.gaps_at = header.gaps_at;
                    self.tiled_to = 0;
                    header
                }
            };
            if self.local == header.row_count {
                if self.tiled_to != self.bytes.len() - header.rows_at {
                    return Err(malformed(format!(
                        "block {} holds {} row bytes but its rows end at {}; a block carries \
                         nothing but whole rows",
                        self.block,
                        self.bytes.len() - header.rows_at,
                        self.tiled_to
                    )));
                }
                self.block += 1;
                self.header = None;
                continue;
            }
            if self.local > 0 {
                let gap = take_varint(&self.bytes[..header.rows_at], &mut self.gaps_at)?;
                self.entity = self
                    .entity
                    .checked_add(gap)
                    .and_then(|e| e.checked_add(1))
                    .ok_or_else(|| {
                        malformed("a block's entity gaps run past the entity ceiling")
                    })?;
            }
            let (start, end) = self
                .blob
                .extent_of(self.block, &header, self.bytes.len(), self.local)?;
            if start - header.rows_at != self.tiled_to {
                return Err(malformed(format!(
                    "block {} row {} starts at byte {} where the previous row ends at {}; rows \
                     must tile the block",
                    self.block,
                    self.local,
                    start - header.rows_at,
                    self.tiled_to
                )));
            }
            let entity = self.entities.current().ok_or_else(|| {
                malformed("the directory addresses more rows than the has-row bitmap holds")
            })?;
            if entity != self.entity {
                return Err(malformed(format!(
                    "block {}'s row {} belongs to entity {} where the has-row bitmap's rank-{} \
                     member is {entity}; the bitmap and the blocks disagree about which entity a \
                     rank names",
                    self.block,
                    self.local,
                    self.entity,
                    header.first_rank as usize + self.local
                )));
            }
            self.entities.move_next();
            let fields = decode_row(&self.bytes[start..end], entity)?;
            self.tiled_to = end - header.rows_at;
            self.local += 1;
            return Ok(Some((entity, fields)));
        }
    }
}

/// A file's bytes as an Arrow buffer — read whole or mapped per `access`, the same construction
/// and safety argument as `values::read_values` and `PostingsReader::open`. Duplicated rather
/// than shared from `values.rs`, which is the scan's hot file and has paid for added code five
/// times over (see `values_writer.rs`'s module doc).
fn read_buffer(path: &Path, access: Access) -> Result<Buffer, RecordError> {
    if access == Access::Read {
        return Ok(Buffer::from_vec(std::fs::read(path)?));
    }
    let file = std::fs::File::open(path)?;
    let len = file.metadata()?.len() as usize;
    if len == 0 {
        // memmap2 rejects zero-length maps, and an empty buffer is what the read path produces
        // for an empty file anyway.
        return Ok(arrow::buffer::MutableBuffer::new(0).into());
    }
    // SAFETY: `arc` owns the mapping for as long as any `Buffer` built from it is alive — it is
    // captured as the buffer's `Allocation` — the mapping is valid for `len` bytes for its whole
    // lifetime, and `memmap2::Mmap` never returns a null base pointer.
    let mapping = unsafe { memmap2::Mmap::map(&file) }?;
    if access == Access::MappedSequential {
        // A hint, and a failure to give it is not a failure to open (decision 0052) — the same
        // posture as `values.rs`'s mapping: the fold streams a layer exactly once and wants the
        // drop-behind, and a kernel that declines leaves a correct mapping behind.
        let _ = mapping.advise(memmap2::Advice::Sequential);
    }
    let len = mapping.len();
    let arc: Arc<memmap2::Mmap> = Arc::new(mapping);
    let ptr = std::ptr::NonNull::new(arc.as_ptr() as *mut u8)
        .expect("memmap2::Mmap never returns a null base pointer");
    Ok(unsafe { Buffer::from_custom_allocation(ptr, len, arc) })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn decode(buf: &[u8], entity: u32) -> Result<Vec<RecordField>, RecordError> {
        decode_row(buf, entity)
    }

    fn field(tag: u16, value: RecordValue) -> RecordField {
        RecordField { tag, value }
    }

    /// Every scalar kind survives the round trip, including the empty string — a value, not an
    /// absence.
    #[test]
    fn a_row_of_every_scalar_kind_round_trips() {
        let fields = vec![
            field(0, RecordValue::Bool(true)),
            field(1, RecordValue::U8(7)),
            field(2, RecordValue::U16(65_535)),
            field(3, RecordValue::U32(4_000_000_000)),
            field(4, RecordValue::U64(u64::MAX)),
            field(5, RecordValue::I8(-8)),
            field(6, RecordValue::I16(-16)),
            field(7, RecordValue::I32(-32)),
            field(8, RecordValue::I64(i64::MIN)),
            field(9, RecordValue::F32(1.5)),
            field(10, RecordValue::F64(-2.25)),
            field(11, RecordValue::TimestampUs(1_700_000_000_000_000)),
            field(
                12,
                RecordValue::Utf8("Bidding on Königsberg's bridges".into()),
            ),
            field(13, RecordValue::Utf8(String::new())),
        ];
        let mut buf = Vec::new();
        encode_row(42, &fields, &mut buf).expect("encode");
        assert_eq!(decode_row(&buf, 42).expect("decode"), fields);
    }

    /// A row is its fields and nothing else: the entity names it in a refusal and is not encoded,
    /// so the same bytes decode under any entity. What says whose row it is is the block header.
    #[test]
    fn a_row_carries_no_entity_of_its_own() {
        let fields = vec![field(0, RecordValue::U8(1))];
        let mut buf = Vec::new();
        encode_row(7, &fields, &mut buf).expect("encode");
        let mut other = Vec::new();
        encode_row(8, &fields, &mut other).expect("encode");
        assert_eq!(buf, other);
        assert_eq!(buf.len(), 4, "tag, kind, value and no header");
    }

    /// The block header round-trips the row count, the first rank and every row's entity, and the
    /// gaps are what the entities are recovered from.
    #[test]
    fn a_block_header_round_trips_its_entities() {
        for entities in [
            vec![0u32],
            vec![7, 8, 9],
            vec![1, 500, 501, 300_000, u32::MAX],
            (0..300).map(|i| i * 3 + 11).collect::<Vec<_>>(),
        ] {
            let mut out = Vec::new();
            encode_block_header(13, &entities, 0xdead_beef_0bad_f00d, &mut out).expect("header");
            let header = decode_block_header(&out, 0).expect("decode");
            assert_eq!(header.row_count, entities.len());
            assert_eq!(header.first_rank, 13);
            assert_eq!(header.first_entity, entities[0]);
            assert_eq!(header.extent_digest, 0xdead_beef_0bad_f00d);
            assert_eq!(header.rows_at, out.len(), "the gaps end where the rows begin");
            for (local, expect) in entities.iter().enumerate() {
                assert_eq!(header.entity_at(&out, local).expect("walk"), *expect);
            }
        }
    }

    /// Strict ascent is not a rule the decoder checks but a shape the encoding cannot express: the
    /// gap stored is the distance less one, so every decoded gap adds at least one. The writer
    /// refuses the input that would need a zero or negative gap.
    #[test]
    fn a_repeated_or_descending_entity_refuses_at_the_header() {
        for entities in [vec![5u32, 5], vec![5, 4], vec![1, 9, 9]] {
            let mut out = Vec::new();
            let err = encode_block_header(0, &entities, 0, &mut out)
                .expect_err("entities ascend strictly");
            assert!(err.to_string().contains("ascending"), "{err}");
        }
    }

    /// An empty block is never written, and the reader refuses one that claims to be.
    #[test]
    fn an_empty_block_refuses_at_both_ends() {
        let mut out = Vec::new();
        assert!(encode_block_header(0, &[], 0, &mut out).is_err());
        let mut crafted = vec![0u8; BLOCK_HEADER_FIXED];
        assert!(decode_block_header(&crafted, 0).is_err());
        crafted.truncate(BLOCK_HEADER_FIXED - 1);
        assert!(decode_block_header(&crafted, 0).is_err());
    }

    /// A header claiming more rows than it carries gaps for refuses on bounds rather than
    /// reading the rows behind it as entity gaps.
    #[test]
    fn a_header_whose_gaps_run_past_the_block_refuses() {
        let mut out = Vec::new();
        encode_block_header(0, &[1, 2, 3], 0, &mut out).expect("header");
        out[0..4].copy_from_slice(&40u32.to_le_bytes());
        let err = decode_block_header(&out, 0).expect_err("the gaps are short");
        assert!(err.to_string().contains("varint"), "{err}");
    }

    /// The extent digest is over the offsets and the rows section's length together, so moving a
    /// row's start, adding a row or changing the section's length all change it.
    #[test]
    fn the_extent_digest_covers_every_offset_and_the_length() {
        let base = extent_digest(&[0, 10, 25], 40);
        assert_ne!(base, extent_digest(&[0, 11, 25], 40));
        assert_ne!(base, extent_digest(&[0, 10, 25], 41));
        assert_ne!(base, extent_digest(&[0, 10, 25, 30], 40));
        assert_ne!(base, extent_digest(&[0, 25, 10], 40));
        assert_eq!(base, extent_digest(&[0, 10, 25], 40));
    }

    /// A truncated payload refuses: the field walk is bounded by the extent the directory gave it.
    #[test]
    fn a_truncated_row_refuses() {
        let mut buf = Vec::new();
        encode_row(3, &[field(0, RecordValue::Utf8("hello".into()))], &mut buf).expect("encode");
        for cut in [buf.len() - 1, 5, 3, 2, 1] {
            let err = decode(&buf[..cut], 3).expect_err("a short row must refuse");
            assert!(
                matches!(err, RecordError::Malformed(_)),
                "cut at {cut}: {err}"
            );
        }
    }

    #[test]
    fn a_duplicate_tag_refuses_at_encode_and_decode() {
        let fields = vec![field(5, RecordValue::U8(1)), field(5, RecordValue::U8(2))];
        let mut buf = Vec::new();
        let err = encode_row(1, &fields, &mut buf).expect_err("duplicate tags refuse");
        assert!(err.to_string().contains("twice"), "{err}");

        // And at decode, over hand-crafted bytes the encoder refuses to produce.
        let crafted = [5u8, 0, KIND_U8, 1, 5, 0, KIND_U8, 2];
        let err = decode(&crafted, 1).expect_err("duplicate tags refuse at decode");
        assert!(err.to_string().contains("twice"), "{err}");
    }

    #[test]
    fn an_empty_row_refuses_at_encode_and_decode() {
        let mut buf = Vec::new();
        assert!(encode_row(1, &[], &mut buf).is_err());
        // A zero-length extent decodes to no fields, which is an absence wearing a row's bytes.
        assert!(decode(&[], 1).is_err());
    }

    /// A bool is 0 or 1; byte 2 is corruption, not `true`.
    #[test]
    fn a_bool_byte_past_one_refuses() {
        let crafted = [0u8, 0, KIND_BOOL, 2];
        let err = decode(&crafted, 1).expect_err("bool byte 2 refuses");
        assert!(err.to_string().contains("bool"), "{err}");
    }

    #[test]
    fn invalid_utf8_refuses() {
        let crafted = [0u8, 0, KIND_UTF8, 2, 0, 0, 0, 0xFF, 0xFE];
        assert!(decode(&crafted, 1).is_err());
    }

    #[test]
    fn an_unknown_kind_byte_refuses() {
        let crafted = [0u8, 0, 200, 0];
        assert!(decode(&crafted, 1).is_err());
    }

    /// The writer refuses a list until epic 3's multi surface lands (records §5).
    #[test]
    fn encoding_a_list_refuses() {
        let mut buf = Vec::new();
        let err = encode_row(
            1,
            &[field(0, RecordValue::List(vec![RecordValue::U8(1)]))],
            &mut buf,
        )
        .expect_err("lists are epic 3's");
        assert!(err.to_string().contains("multi"), "{err}");
    }

    /// The list encoding is pinned from crafted bytes, since no writer may produce it yet: the
    /// element kind once, a count, then bare element encodings — strings keeping their own length
    /// prefixes.
    #[test]
    fn the_specified_list_encoding_decodes() {
        let mut crafted: Vec<u8> = vec![3, 0, KIND_LIST, KIND_U16, 3, 0, 0, 0];
        for v in [10u16, 20, 30] {
            crafted.extend_from_slice(&v.to_le_bytes());
        }
        crafted.extend_from_slice(&[7, 0, KIND_LIST, KIND_UTF8, 2, 0, 0, 0]);
        for s in ["ab", ""] {
            crafted.extend_from_slice(&(s.len() as u32).to_le_bytes());
            crafted.extend_from_slice(s.as_bytes());
        }

        let decoded = decode(&crafted, 9).expect("the specified encoding decodes");
        assert_eq!(
            decoded,
            vec![
                field(
                    3,
                    RecordValue::List(vec![
                        RecordValue::U16(10),
                        RecordValue::U16(20),
                        RecordValue::U16(30),
                    ])
                ),
                field(
                    7,
                    RecordValue::List(vec![
                        RecordValue::Utf8("ab".into()),
                        RecordValue::Utf8(String::new()),
                    ])
                ),
            ]
        );
    }

    /// A nested list refuses — the multi model is one level deep — and a list whose count runs
    /// past the payload refuses by bounds rather than allocating.
    ///
    /// **Both refusals are read by message, because `is_err()` cannot tell them apart.** The
    /// nested payload carries no bytes for the inner list's own header, so a decoder with the
    /// depth guard removed runs out of payload and refuses on truncation instead — a different
    /// fault, the same `is_err()`. The two assertions here are therefore mirrored: the nested case
    /// must name the depth rule and must *not* be a bounds refusal, and the overrunning case the
    /// other way round.
    ///
    /// Mutations this kills: passing `true` for `lists_allowed` when decoding a list's elements,
    /// or deleting the guard outright — either leaves unbounded recursion in `decode_value` over a
    /// crafted bundle, which is a stack overflow rather than a refusal.
    #[test]
    fn a_nested_or_overrunning_list_refuses() {
        let nested = [0u8, 0, KIND_LIST, KIND_LIST, 1, 0, 0, 0];
        let err = decode(&nested, 1)
            .expect_err("a list element that is itself a list refuses")
            .to_string();
        assert!(
            err.contains("one level deep"),
            "the depth rule must be what refuses, not a truncation behind it: {err}"
        );
        assert!(
            !err.contains("runs past the end"),
            "a bounds refusal here would mean the depth guard never ran: {err}"
        );

        // Claims 2^32 - 1 u64 elements and supplies none.
        let overrun = [0u8, 0, KIND_LIST, KIND_U64, 0xFF, 0xFF, 0xFF, 0xFF];
        let err = decode(&overrun, 1)
            .expect_err("a list claiming more elements than the payload holds refuses")
            .to_string();
        assert!(
            err.contains("runs past the end"),
            "the count must be refused by bounds rather than allocated for: {err}"
        );
    }
}
