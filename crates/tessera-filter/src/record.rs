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
//!            | gap × (row_count - 1) | row × row_count
//! gap     := LEB128 varint: the row's entity less its predecessor's, less one
//! row     := payload_len LEB128 varint | field*
//! field   := tag u16 LE | kind u8 | value
//! kinds   := 0 bool (one byte, 0 or 1 — anything else refuses)
//!            1 u8   2 u16   3 u32   4 u64   5 i8   6 i16   7 i32   8 i64
//!            9 f32  10 f64  11 timestamp_us (i64 LE)
//!            12 utf8 (byte_len u32 LE | bytes, validated UTF-8)
//!            13 list (elem_kind u8 | count u32 LE | count values, element encoding only)
//! ```
//!
//! A row states how many bytes of fields follow it and nothing else; its fields must consume
//! exactly that many. Rows therefore tile a block's rows section end to end and are reached by
//! walking it, which costs a varint and an addition apiece over rows already in cache. Rows carry
//! no entity id of their own: the block states the first entity once and one varint per row after
//! it, so the identity of every row is in the block that holds it at about a byte apiece rather
//! than four ([decision 0141](../../../docs/decisions/0141-the-record-blob-states-identity-once-per-block.md)).
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
//! point on *title-shaped* bytes (2.44× at 169 µs/read), and the mixed-field row ratio records §3
//! once marked assumed is measured there at 3.00× against a 2.54× title control through this
//! writer. Neither figure is quoted here as this format's: both were taken before the row form
//! moved twice.
//!
//! # Addressing is has-row rank (records §3, review B5)
//!
//! A **has-row Roaring bitmap** marks the entities that have a row; a block directory of
//! `(compressed offset, first rank)` locates the block holding an entity's rank by binary search;
//! the row is then the rank less the block's first rank rows into the block's rows section. An
//! entity with no blob-resident value is absent from the bitmap and occupies nothing anywhere.
//! Both obvious alternatives were wrong: a per-entity offset array pays 4 B for every entity that
//! has no row, and a per-field presence structure re-buys per-column addressing for bytes that
//! share a row.
//!
//! On disk the directory is one Arrow batch, one row per block: the compressed offset **and
//! length**, the uncompressed length, the first rank **and the row count**. It holds nothing per
//! row. A rank-indexed
//! array of within-block offsets would be 4 B for every row in the blob, held resident for the
//! whole of a build's blob stage and of every read after it — 14 GB over the 3.5×10⁹-row GBIF
//! rung against a 13.9 GB `blocks.bin` — and it buys no read: reaching any row in a block costs
//! that block's decompress, and walking the rows section past `k` rows costs a varint and an
//! addition each over bytes already in cache. So the delimiter lives in the row, where it is a
//! varint inside the block's own compression rather than four uncompressed bytes in a second file.
//! The compressed lengths are derivable from the neighbouring offsets and are carried anyway:
//! redundancy the reader *checks* is what turns an addressing defect into a refusal instead of a
//! wrong answer.
//!
//! # The read is fail-closed (records §3, review B6)
//!
//! The blob's indirection is the one new failure class the attribute artefacts acquire: the other
//! homes are positional, so there is no offset to get wrong. A build or fold defect here would
//! otherwise serve a *neighbour's* record for a visible entity, out of blocks that also hold
//! entities the principal cannot see. Reaching a row is two derived indirections across files —
//! the has-row rank and the block that holds it — and each is checked against something the other
//! file states. The third, where the row sits inside the block, is no longer derived from a
//! second file at all: the delimiters are the block's own bytes, so the class of defect where two
//! files address past each other has nothing to address with.
//!
//! **The block header is that something.** Its `first_rank` is the directory's claim restated in
//! the bytes the directory addresses, so a corrupt compressed offset or a mis-chosen block refuses
//! instead of answering. Its `first_entity` is the has-row bitmap's rank-`first_rank` member
//! restated in the block, so a corrupt bitmap refuses rather than shifting every row of the blob by
//! one. Its `row_count` is the count the directory's first ranks imply for the block, so a
//! directory whose blocks and ranks disagree refuses before a row is read. The gaps then give each
//! row its own entity, checked against the entity the caller resolved through the rank, so a wrong
//! rank inside the right block refuses too. Every row length is bounds-checked against its block
//! as the walk reaches it, the walk must reach the block's end exactly at its last row, and any
//! mismatch, short file or malformed directory is a typed [`RecordError`] — never a neighbour's
//! row, never a silent absence.
//!
//! Entity ids in the block are an index internal and are never serialised to any client (**I10**
//! as corrected by decision 0065: the blob is an index internal, not a gather artefact).
//!
//! **What this does not catch**: a block's rows section rewritten so that it still tiles. The
//! field walk is what stands inside a row — a kind byte naming no kind, a length running past the
//! block, a walk that ends short of the row's own length all refuse — and the tiling walk stands
//! around it, but bytes that satisfy both do not refuse. Someone who rewrites a block may re-cut
//! its rows as well as their contents: as long as the walk visits the row count the header states
//! and lands on the block's last byte, the entities still come from the gaps, which are untouched,
//! so a neighbour's bytes can be presented under an earlier entity's identity where the tags
//! permit. **The manifest's SHA-256 over `blocks.bin` is what stands there** (records §7: a blob
//! file that fails its digest refuses at open), and decision 0142 leaves open whether that takes a
//! leak-register row. [`RecordBlob::self_check`] walks the whole artefact — ranks, the rows tiling
//! their block, block bounds, identities — for the conformance suite and for
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

use arrow::array::{Array, UInt32Array, UInt64Array};
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

/// A field's value **borrowed from the bytes it came out of**: the form every producer of a blob
/// row speaks, and the form the row walk yields.
///
/// A string is where this matters. A merge reading rows out of one blob and writing them into
/// another touches every character three times if the value is owned — out of the decompressed
/// block into a `String`, out of the `String` into the row buffer, and the free — and once if it
/// is borrowed. Over the 3.5×10⁹-row GBIF rung the owned form was 6.4×10⁹ allocate-and-free
/// pairs across the blob's merge and the keyword pass that reads the same extents. Lists are
/// absent here because no writer produces one (records §5); a decoded list becomes a
/// [`RecordValue`].
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum RecordValueRef<'a> {
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
    TimestampUs(i64),
    Utf8(&'a str),
}

/// One field of a row, its value borrowed ([`RecordValueRef`]).
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct RecordFieldRef<'a> {
    /// The column's position in the manifest's `declared_scalars`.
    pub tag: u16,
    pub value: RecordValueRef<'a>,
}

impl RecordValue {
    /// This value borrowed, or `None` for a list — which [`encode_row`] refuses anyway, the multi
    /// surface not having landed (records §5).
    pub fn as_ref(&self) -> Option<RecordValueRef<'_>> {
        Some(match self {
            RecordValue::Bool(v) => RecordValueRef::Bool(*v),
            RecordValue::U8(v) => RecordValueRef::U8(*v),
            RecordValue::U16(v) => RecordValueRef::U16(*v),
            RecordValue::U32(v) => RecordValueRef::U32(*v),
            RecordValue::U64(v) => RecordValueRef::U64(*v),
            RecordValue::I8(v) => RecordValueRef::I8(*v),
            RecordValue::I16(v) => RecordValueRef::I16(*v),
            RecordValue::I32(v) => RecordValueRef::I32(*v),
            RecordValue::I64(v) => RecordValueRef::I64(*v),
            RecordValue::F32(v) => RecordValueRef::F32(*v),
            RecordValue::F64(v) => RecordValueRef::F64(*v),
            RecordValue::TimestampUs(v) => RecordValueRef::TimestampUs(*v),
            RecordValue::Utf8(v) => RecordValueRef::Utf8(v.as_str()),
            RecordValue::List(_) => return None,
        })
    }
}

impl RecordValueRef<'_> {
    /// This value owned, for a caller that keeps it past the bytes it borrows.
    pub fn to_owned(&self) -> RecordValue {
        match *self {
            RecordValueRef::Bool(v) => RecordValue::Bool(v),
            RecordValueRef::U8(v) => RecordValue::U8(v),
            RecordValueRef::U16(v) => RecordValue::U16(v),
            RecordValueRef::U32(v) => RecordValue::U32(v),
            RecordValueRef::U64(v) => RecordValue::U64(v),
            RecordValueRef::I8(v) => RecordValue::I8(v),
            RecordValueRef::I16(v) => RecordValue::I16(v),
            RecordValueRef::I32(v) => RecordValue::I32(v),
            RecordValueRef::I64(v) => RecordValue::I64(v),
            RecordValueRef::F32(v) => RecordValue::F32(v),
            RecordValueRef::F64(v) => RecordValue::F64(v),
            RecordValueRef::TimestampUs(v) => RecordValue::TimestampUs(v),
            RecordValueRef::Utf8(v) => RecordValue::Utf8(v.to_string()),
        }
    }
}

impl RecordFieldRef<'_> {
    /// This field owned.
    pub fn to_owned(&self) -> RecordField {
        RecordField {
            tag: self.tag,
            value: self.value.to_owned(),
        }
    }
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

/// A block's fixed header: the row count, the first rank and the first entity, four bytes each.
/// The per-row entity gaps follow it, and the rows follow those.
const BLOCK_HEADER_FIXED: usize = 12;

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
/// found, the end of that section once it is known, and the whole block again for the length that
/// heads a row.
fn take_varint(bytes: &[u8], cursor: &mut usize) -> Result<u32, RecordError> {
    let mut value: u32 = 0;
    let mut shift = 0u32;
    loop {
        let Some(byte) = bytes.get(*cursor).copied() else {
            return Err(malformed("a block ends in the middle of a varint"));
        };
        *cursor += 1;
        let payload = u32::from(byte & 0x7f);
        let Some(shifted) = payload
            .checked_shl(shift)
            .filter(|v| (v >> shift) == payload)
        else {
            return Err(malformed("a block's varint does not fit u32"));
        };
        value |= shifted;
        if byte & 0x80 == 0 {
            if shift > 0 && byte == 0 {
                return Err(malformed(
                    "a block's varint is padded with a redundant continuation byte",
                ));
            }
            return Ok(value);
        }
        shift += 7;
        if shift >= 32 {
            return Err(malformed("a block's varint does not fit u32"));
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
    let gaps_at = BLOCK_HEADER_FIXED;
    let mut cursor = gaps_at;
    for _ in 1..row_count {
        take_varint(bytes, &mut cursor)?;
    }
    Ok(BlockHeader {
        row_count,
        first_rank,
        first_entity,
        gaps_at,
        rows_at: cursor,
    })
}

/// A position in one decompressed block: which row is next, where its length varint begins, and
/// the entity the block's gaps say that row belongs to.
///
/// **Every read of a row goes through one of these.** A block's rows are delimited by their own
/// lengths and its entities by their own gaps, so a row is reached by walking to it, and the walk
/// is where the bounds and the identity are checked. Holding the position lets a reader that wants
/// several rows of one block walk it once: [`RecordBlob::for_each_row_in`] and
/// [`RecordRowCursor`] carry a scan across rows, and [`RecordBlob::fields_of`], which wants one
/// row and holds nothing between calls, starts a fresh one.
#[derive(Debug, Clone, Copy)]
struct BlockScan {
    /// The index within the block of the row the scan is at.
    local: usize,
    /// Where that row's length varint begins in the uncompressed block.
    at: usize,
    /// The entity the gaps give that row.
    entity: u32,
    /// Where the gap walk has reached.
    gaps_at: usize,
}

impl BlockScan {
    /// The scan at a block's first row.
    fn start(header: &BlockHeader) -> Self {
        BlockScan {
            local: 0,
            at: header.rows_at,
            entity: header.first_entity,
            gaps_at: header.gaps_at,
        }
    }

    /// Where the current row's fields begin and end in the block, bounds-checked against it.
    ///
    /// The length is the row's own statement, so a length that runs past the block refuses here
    /// rather than reading a neighbour's bytes, and a zero length refuses because an entity with
    /// no field has no row at all.
    fn extent(&self, bytes: &[u8], block: usize) -> Result<(usize, usize), RecordError> {
        let mut cursor = self.at;
        let len = take_varint(bytes, &mut cursor)? as usize;
        let end = cursor
            .checked_add(len)
            .filter(|end| *end <= bytes.len())
            .ok_or_else(|| {
                malformed(format!(
                    "block {block} row {} states {len} bytes of fields, which run past the \
                     {}-byte block",
                    self.local,
                    bytes.len()
                ))
            })?;
        if len == 0 {
            return Err(malformed(format!(
                "block {block} row {} states no fields; an entity with no blob-resident value \
                 has no row at all",
                self.local
            )));
        }
        Ok((cursor, end))
    }

    /// Step past the current row to the next, taking that row's entity from the gaps.
    fn advance(
        &mut self,
        bytes: &[u8],
        header: &BlockHeader,
        block: usize,
    ) -> Result<(), RecordError> {
        let (_, end) = self.extent(bytes, block)?;
        self.at = end;
        self.local += 1;
        if self.local < header.row_count {
            let gap = take_varint(&bytes[..header.rows_at], &mut self.gaps_at)?;
            self.entity = self
                .entity
                .checked_add(gap)
                .and_then(|e| e.checked_add(1))
                .ok_or_else(|| malformed("a block's entity gaps run past the entity ceiling"))?;
        }
        Ok(())
    }
}

/// Append one entity's row to `out` — the writer's half of the format, kept beside the decoder so
/// the layout exists in one module. A row is how many bytes of fields follow and then those
/// fields; the block header states whose row it is.
///
/// The length is written as a LEB128 varint ahead of the fields, which costs one byte on a row
/// under 128 bytes and two under 16 KiB, inside the block's own compression. It is what delimits
/// the row: the alternative is a rank-indexed offset in the directory, four uncompressed bytes a
/// row in a second file that a reader must hold resident and check against these bytes.
///
/// Refuses: an empty field list (a row with no fields is an absence, and absence is absence from
/// the has-row bitmap — see records §3); a duplicate tag; and a list value, which
/// [`RecordValueRef`] cannot spell (specified for epic 3, produced by nothing until the multi
/// surface lands — records §5). `entity` names the row in those refusals and is not encoded.
pub fn encode_row(
    entity: u32,
    fields: &[RecordFieldRef<'_>],
    out: &mut Vec<u8>,
) -> Result<(), RecordError> {
    for (i, field) in fields.iter().enumerate() {
        if fields[..i].iter().any(|f| f.tag == field.tag) {
            return Err(malformed(format!(
                "entity {entity} carries field tag {} twice; a field's value is one value",
                field.tag
            )));
        }
    }
    encode_row_from(entity, fields.iter().copied(), out)
}

/// [`encode_row`] over a stream of fields that **ascend strictly in the tag**, which is what a
/// merge has in hand once it has ordered one entity's fields: the ascent is the duplicate check,
/// so the row is encoded without being collected first.
pub fn encode_row_from<'a>(
    entity: u32,
    fields: impl Iterator<Item = RecordFieldRef<'a>>,
    out: &mut Vec<u8>,
) -> Result<(), RecordError> {
    let mut fields = fields;
    encode_row_with(entity, out, &mut |row| {
        for field in fields.by_ref() {
            row.field(field)?;
        }
        Ok(())
    })
}

/// [`encode_row`] where the fields are handed over one at a time, in ascending tag order, by a
/// producer that cannot collect them first.
///
/// A merge over several row streams is that producer: the fields of one entity's row are spread
/// across streams that each own the bytes they lend, so a collected row would be a vector of
/// borrows of several streams at once and could not be reused between rows. Handing them over one
/// at a time encodes straight into the block buffer and holds nothing.
pub fn encode_row_with(
    entity: u32,
    out: &mut Vec<u8>,
    fields: &mut dyn FnMut(&mut RowFields<'_>) -> Result<(), RecordError>,
) -> Result<(), RecordError> {
    let start = out.len();
    let mut row = RowFields {
        out,
        entity,
        last: None,
    };
    let result = fields(&mut row);
    let any = row.last.is_some();
    if let Err(e) = result {
        out.truncate(start);
        return Err(e);
    }
    if !any {
        out.truncate(start);
        return Err(malformed(format!(
            "entity {entity} was given a row with no fields; an entity with no blob-resident \
             value has no row at all (records §3)"
        )));
    }
    let Ok(len) = u32::try_from(out.len() - start) else {
        out.truncate(start);
        return Err(malformed(format!(
            "entity {entity}'s row is past u32::MAX bytes of fields"
        )));
    };
    // The length is known only once the fields are encoded, and its own width only once it is
    // known, so it is appended and rotated to the front of the row it heads. The rotation moves
    // the row's bytes and nothing before them.
    let before = out.len();
    put_varint(len, out);
    let width = out.len() - before;
    out[start..].rotate_right(width);
    Ok(())
}

/// One row's fields being appended, in ascending tag order ([`encode_row_with`]).
pub struct RowFields<'o> {
    out: &'o mut Vec<u8>,
    entity: u32,
    last: Option<u16>,
}

impl RowFields<'_> {
    /// Append one field. Tags must ascend strictly; a tag at or below its predecessor is a field
    /// with two values, or a producer that did not order them.
    pub fn field(&mut self, field: RecordFieldRef<'_>) -> Result<(), RecordError> {
        if self.last.is_some_and(|last| last >= field.tag) {
            return Err(malformed(format!(
                "entity {}'s fields arrived at tag {} after tag {}; a row's fields ascend \
                 strictly and a repeated tag is a field with two values",
                self.entity,
                field.tag,
                self.last.expect("checked is_some")
            )));
        }
        self.last = Some(field.tag);
        self.out.extend_from_slice(&field.tag.to_le_bytes());
        encode_value(&field.value, self.out);
        Ok(())
    }
}

fn encode_value(value: &RecordValueRef<'_>, out: &mut Vec<u8>) {
    macro_rules! fixed {
        ($kind:expr, $v:expr) => {{
            out.push($kind);
            out.extend_from_slice(&$v.to_le_bytes());
        }};
    }
    match *value {
        RecordValueRef::Bool(b) => out.extend_from_slice(&[KIND_BOOL, u8::from(b)]),
        RecordValueRef::U8(v) => out.extend_from_slice(&[KIND_U8, v]),
        RecordValueRef::U16(v) => fixed!(KIND_U16, v),
        RecordValueRef::U32(v) => fixed!(KIND_U32, v),
        RecordValueRef::U64(v) => fixed!(KIND_U64, v),
        RecordValueRef::I8(v) => fixed!(KIND_I8, v),
        RecordValueRef::I16(v) => fixed!(KIND_I16, v),
        RecordValueRef::I32(v) => fixed!(KIND_I32, v),
        RecordValueRef::I64(v) => fixed!(KIND_I64, v),
        RecordValueRef::F32(v) => fixed!(KIND_F32, v),
        RecordValueRef::F64(v) => fixed!(KIND_F64, v),
        RecordValueRef::TimestampUs(v) => fixed!(KIND_TIMESTAMP_US, v),
        RecordValueRef::Utf8(s) => {
            // A string past u32::MAX is unreachable: the row it sits in is bounded by the same
            // width and [`encode_row_from`] refuses there, so this saturates rather than
            // carrying a second refusal for the same condition.
            let len = u32::try_from(s.len()).unwrap_or(u32::MAX);
            out.push(KIND_UTF8);
            out.extend_from_slice(&len.to_le_bytes());
            out.extend_from_slice(&s.as_bytes()[..len as usize]);
        }
    }
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

/// One field's value out of `payload`, borrowed. A list refuses here: the multi surface has not
/// landed, so no artefact carries one, and the borrowing walk is the hot path
/// ([`decode_row_fields`]). [`decode_row`] is the route that decodes one.
fn decode_value_ref<'a>(
    payload: &'a [u8],
    cursor: &mut usize,
    kind: u8,
) -> Result<RecordValueRef<'a>, RecordError> {
    macro_rules! fixed {
        ($variant:ident, $t:ty, $what:expr) => {{
            let bytes = take!(payload, cursor, std::mem::size_of::<$t>(), $what);
            RecordValueRef::$variant(<$t>::from_le_bytes(
                bytes.try_into().expect("take! returned the exact width"),
            ))
        }};
    }
    Ok(match kind {
        KIND_BOOL => {
            let byte = take!(payload, cursor, 1, "bool value")[0];
            match byte {
                0 => RecordValueRef::Bool(false),
                1 => RecordValueRef::Bool(true),
                other => {
                    return Err(malformed(format!(
                        "a bool field holds byte {other}; only 0 and 1 are bools"
                    )))
                }
            }
        }
        KIND_U8 => RecordValueRef::U8(take!(payload, cursor, 1, "u8 value")[0]),
        KIND_U16 => fixed!(U16, u16, "u16 value"),
        KIND_U32 => fixed!(U32, u32, "u32 value"),
        KIND_U64 => fixed!(U64, u64, "u64 value"),
        KIND_I8 => RecordValueRef::I8(take!(payload, cursor, 1, "i8 value")[0] as i8),
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
            RecordValueRef::Utf8(
                std::str::from_utf8(bytes)
                    .map_err(|_| malformed("a utf8 field holds bytes that are not UTF-8"))?,
            )
        }
        KIND_LIST => {
            return Err(malformed(
                "a row carries a list value; the list encoding is specified but populated only \
                 when the multi surface lands (records §5)",
            ))
        }
        other => {
            return Err(malformed(format!(
                "a field carries kind byte {other}, which names no value kind"
            )))
        }
    })
}

/// Walk one row's fields into `out` as `(tag, kind, value start, value end)`, positions relative
/// to `payload`.
///
/// **The walk a row cursor makes, which holds no borrow of the bytes it walked.** A cursor owns
/// the decompressed block it reads, so it cannot hold decoded values pointing into it; it holds
/// these positions instead and builds a [`RecordFieldRef`] when a caller asks for a field. The
/// table is reused row after row, so a whole blob walks with no allocation past the first row.
///
/// Every check [`decode_row`] makes is made here: the walk consumes the row exactly, a duplicate
/// tag refuses, and each value is decoded as it is passed so an unknown kind or an out-of-bounds
/// length refuses now rather than when a field is read.
fn decode_row_fields(
    payload: &[u8],
    entity: u32,
    out: &mut Vec<(u16, u8, u32, u32)>,
) -> Result<(), RecordError> {
    out.clear();
    let mut cursor = 0usize;
    while cursor < payload.len() {
        let tag = take!(payload, &mut cursor, 2, "field tag");
        let tag = u16::from_le_bytes(tag.try_into().expect("two bytes"));
        if out.iter().any(|held| held.0 == tag) {
            return Err(malformed(format!(
                "entity {entity}'s row carries field tag {tag} twice"
            )));
        }
        let kind = take!(payload, &mut cursor, 1, "field kind")[0];
        let start = cursor;
        decode_value_ref(payload, &mut cursor, kind)?;
        let (Ok(start), Ok(end)) = (u32::try_from(start), u32::try_from(cursor)) else {
            return Err(malformed(format!(
                "entity {entity}'s row is past u32::MAX bytes"
            )));
        };
        out.push((tag, kind, start, end));
    }
    if out.is_empty() {
        return Err(malformed(format!(
            "entity {entity}'s row carries no fields; an entity with no blob-resident value has \
             no row at all"
        )));
    }
    Ok(())
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

/// Decode one row's fields out of the bytes its own length covers.
///
/// That length is the whole of the row: the field walk must consume it exactly. A walk that runs
/// past it refuses on bounds, and one that ends short of it cannot — the loop runs until the
/// cursor reaches the end — so a length that does not frame a whole field sequence refuses.
/// `entity` names the row in the refusals; it is checked against the block's own statement of
/// identity before this is called.
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
/// request reads through it. What open cannot see (a block's rows tiling it, the discriminants) is
/// checked when the block is decompressed and exhaustively by [`Self::self_check`].
#[derive(Debug)]
pub struct RecordBlob {
    hasrow: Bitmap,
    blocks: Buffer,
    compressed_offset: ScalarBuffer<u64>,
    compressed_len: ScalarBuffer<u64>,
    uncompressed_len: ScalarBuffer<u32>,
    first_rank: ScalarBuffer<u32>,
    /// How many rows each block holds. Derivable from the neighbouring first ranks and carried
    /// anyway, at four bytes a block: it is what lets the has-row cardinality be checked against
    /// the directory at open rather than at the first read of the last block.
    row_count: ScalarBuffer<u32>,
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
        let row_count = dir_column_u32(4)?.values().clone();
        if (0..batch.num_columns()).any(|i| batch.column(i).null_count() != 0) {
            return Err(malformed(
                "the block directory carries nulls; every directory value is required",
            ));
        }

        let blob = RecordBlob {
            hasrow,
            blocks,
            compressed_offset,
            compressed_len,
            uncompressed_len,
            first_rank,
            row_count,
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
        if self.compressed_len.len() != blocks
            || self.uncompressed_len.len() != blocks
            || self.first_rank.len() != blocks
            || self.row_count.len() != blocks
        {
            return Err(malformed(
                "the block directory's columns are of unequal length",
            ));
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

    /// How many rows block `block` holds, as the directory states it. The block's own header
    /// states it too, and [`Self::header_of`] compares them.
    fn rows_in_block(&self, block: usize) -> Result<usize, RecordError> {
        let rows = self.row_count[block] as usize;
        if rows == 0 {
            return Err(malformed(format!(
                "block {block} holds no rows; an empty block is never written"
            )));
        }
        Ok(rows)
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
    /// the block — the directory's row count and first rank, the has-row bitmap's member at that
    /// rank — and against the block's own bytes through the tiling walk below. Past this, the
    /// block and the files that address it are known to be describing the same rows, and every
    /// row of the block is known to be reachable inside it.
    fn header_of(&self, block: usize, bytes: &[u8]) -> Result<BlockHeader, RecordError> {
        let header = decode_block_header(bytes, block)?;
        let rows = self.rows_in_block(block)?;
        // The count the block states against the count the directory's first ranks imply. It is
        // what bounds the walk: every row a block claims must be reachable inside it, and the
        // last must end exactly where the block does.
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
        // **The rows tile the block, checked once per block rather than once per row.** Each row
        // states how many bytes of fields follow it, so the block's delimiters are checkable
        // against nothing but the block: walking exactly `row_count` of them must land on the
        // block's last byte. Without this a length doctored to swallow its neighbour would put a
        // later row's bytes under an earlier row's entity, which is the substitution records §3
        // makes a refusal. It costs a varint a row over bytes just decompressed, against the
        // decompress itself.
        let mut scan = BlockScan::start(&header);
        for _ in 0..header.row_count {
            scan.advance(bytes, &header, block)?;
        }
        if scan.at != bytes.len() {
            return Err(malformed(format!(
                "block {block} holds {} row bytes but its {} rows end at {}; a block carries \
                 nothing but whole rows",
                bytes.len() - header.rows_at,
                header.row_count,
                scan.at - header.rows_at
            )));
        }
        Ok(header)
    }

    /// One row out of an already-decompressed block, addressed by its rank, walking `scan`
    /// forward to it.
    ///
    /// **The per-read half of the fail-closed rule, stated once.** Both readers that address a
    /// single row — [`Self::fields_of`] and [`Self::for_each_row_in`] — come through here, so the
    /// identity check cannot hold on one route and not the other. `scan` is where the caller has
    /// already walked to in this block; it never walks backwards, which is what makes a reader
    /// wanting many rows of one block cost one walk rather than one per row.
    fn row_at(
        &self,
        bytes: &[u8],
        block: usize,
        header: &BlockHeader,
        scan: &mut BlockScan,
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
        if local < scan.local {
            return Err(malformed(format!(
                "rank {rank} was addressed into block {block} behind the row {} the walk has \
                 reached; a block is read forwards",
                scan.local
            )));
        }
        while scan.local < local {
            scan.advance(bytes, header, block)?;
        }
        if scan.entity != entity {
            return Err(malformed(format!(
                "the row addressed for entity {entity} is block {block}'s row {local}, which the \
                 block says belongs to entity {}; the addressing is inconsistent and the \
                 request is refused rather than answered with another entity's record \
                 (records §3, review B6)",
                scan.entity
            )));
        }
        let (start, end) = scan.extent(bytes, block)?;
        decode_row(&bytes[start..end], entity)
    }

    /// The whole row `entity` carries, decoded — or `None` where it has none.
    ///
    /// One block read and one decompress (records §3's cost shape). Every failure of the
    /// addressing — a length running past the block, rows that do not account for the block, a
    /// discriminant naming another entity — refuses rather than answers.
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
        let mut scan = BlockScan::start(&header);
        self.row_at(&bytes, block, &header, &mut scan, rank, entity)
            .map(Some)
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
        let mut loaded: Option<(usize, BlockHeader, BlockScan)> = None;
        let mut bytes: Vec<u8> = Vec::new();
        for entity in present.iter() {
            let rank = (self.hasrow.rank(entity) - 1) as u32;
            let block = self.block_of(rank);
            if loaded.map(|(b, _, _)| b) != Some(block) {
                bytes = self.block_bytes(block)?;
                let header = self.header_of(block, &bytes)?;
                let scan = BlockScan::start(&header);
                loaded = Some((block, header, scan));
            }
            let (_, header, scan) = loaded.as_mut().expect("just loaded");
            let header = *header;
            f(
                entity,
                self.row_at(&bytes, block, &header, scan, rank, entity)?,
            )?;
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
    /// The open block's bytes, its checked header, and where the walk has reached in it.
    bytes: Vec<u8>,
    header: Option<BlockHeader>,
    scan: BlockScan,
    /// The row the cursor is at: its entity, and its fields as positions into `bytes`. The table
    /// is reused row after row ([`decode_row_fields`]), so a walk allocates once.
    row: Option<(u32, usize)>,
    fields: Vec<(u16, u8, u32, u32)>,
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
            scan: BlockScan {
                local: 0,
                at: 0,
                entity: 0,
                gaps_at: 0,
            },
            row: None,
            fields: Vec::new(),
            whole,
        }
    }

    /// The next row, or `None` at the end of the cursor's blocks.
    ///
    /// The owned form, for a caller that keeps the row past the next block. A merge wants the
    /// borrowed one ([`Self::advance`]).
    #[allow(clippy::should_implement_trait)]
    pub fn next_row(&mut self) -> Result<Option<(u32, Vec<RecordField>)>, RecordError> {
        if !self.advance()? {
            return Ok(None);
        }
        let entity = self.entity();
        let fields = (0..self.field_count())
            .map(|i| self.field(i).map(|f| f.to_owned()))
            .collect::<Result<Vec<_>, _>>()?;
        Ok(Some((entity, fields)))
    }

    /// Walk to the next row, or `Ok(false)` at the end of the cursor's blocks. The row's fields
    /// are then [`Self::field_count`] and [`Self::field`], borrowed from the block the cursor
    /// holds and valid until the next call.
    pub fn advance(&mut self) -> Result<bool, RecordError> {
        self.row = None;
        loop {
            if self.block >= self.end_block {
                if self.whole && self.entities.has_value() {
                    return Err(malformed(
                        "the has-row bitmap holds entities the directory never addresses",
                    ));
                }
                return Ok(false);
            }
            let header = match self.header {
                Some(header) => header,
                None => {
                    self.bytes = self.blob.block_bytes(self.block)?;
                    let header = self.blob.header_of(self.block, &self.bytes)?;
                    self.header = Some(header);
                    self.scan = BlockScan::start(&header);
                    header
                }
            };
            if self.scan.local == header.row_count {
                // The rows tile the block, which `header_of` proved of these bytes before the
                // first row was read. Reaching the block's end here is the cursor's own walk
                // agreeing with that.
                if self.scan.at != self.bytes.len() {
                    return Err(malformed(format!(
                        "block {} holds {} row bytes but its rows end at {}; a block carries \
                         nothing but whole rows",
                        self.block,
                        self.bytes.len() - header.rows_at,
                        self.scan.at - header.rows_at
                    )));
                }
                self.block += 1;
                self.header = None;
                continue;
            }
            let entity = self.entities.current().ok_or_else(|| {
                malformed("the directory addresses more rows than the has-row bitmap holds")
            })?;
            if entity != self.scan.entity {
                return Err(malformed(format!(
                    "block {}'s row {} belongs to entity {} where the has-row bitmap's rank-{} \
                     member is {entity}; the bitmap and the blocks disagree about which entity a \
                     rank names",
                    self.block,
                    self.scan.local,
                    self.scan.entity,
                    header.first_rank as usize + self.scan.local
                )));
            }
            self.entities.move_next();
            let (start, end) = self.scan.extent(&self.bytes, self.block)?;
            decode_row_fields(&self.bytes[start..end], entity, &mut self.fields)?;
            self.scan.advance(&self.bytes, &header, self.block)?;
            self.row = Some((entity, start));
            return Ok(true);
        }
    }

    /// The entity of the row the cursor is at. Zero before the first [`Self::advance`].
    pub fn entity(&self) -> u32 {
        self.row.map_or(0, |(entity, _)| entity)
    }

    /// How many fields the row the cursor is at carries.
    pub fn field_count(&self) -> usize {
        if self.row.is_some() {
            self.fields.len()
        } else {
            0
        }
    }

    /// Field `i` of the row the cursor is at, borrowed from the block the cursor holds.
    ///
    /// The value is decoded here rather than at [`Self::advance`] because a decoded value borrows
    /// the block and the cursor owns it. Every check is already made at advance, so the only
    /// failure reachable here is a caller asking for a field the row does not carry.
    pub fn field(&self, i: usize) -> Result<RecordFieldRef<'_>, RecordError> {
        let (entity, at) = self
            .row
            .ok_or_else(|| malformed("a field was asked for before the cursor reached a row"))?;
        let &(tag, kind, start, end) = self.fields.get(i).ok_or_else(|| {
            malformed(format!(
                "field {i} was asked for of entity {entity}'s row, which carries {}",
                self.fields.len()
            ))
        })?;
        let payload = &self.bytes[at + start as usize..at + end as usize];
        let mut cursor = 0usize;
        let value = decode_value_ref(payload, &mut cursor, kind)?;
        Ok(RecordFieldRef { tag, value })
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

    /// The fields of an encoded row, less the length that heads it — and the length checked
    /// against what follows, which is the whole of the delimiter's contract.
    fn payload(buf: &[u8]) -> &[u8] {
        let mut cursor = 0usize;
        let len = take_varint(buf, &mut cursor).expect("the row states its length") as usize;
        assert_eq!(
            cursor + len,
            buf.len(),
            "a row's length covers its fields exactly"
        );
        &buf[cursor..]
    }

    fn field(tag: u16, value: RecordValue) -> RecordField {
        RecordField { tag, value }
    }

    /// Owned fields as the encoder takes them.
    fn borrowed(fields: &[RecordField]) -> Vec<RecordFieldRef<'_>> {
        fields
            .iter()
            .map(|f| RecordFieldRef {
                tag: f.tag,
                value: f.value.as_ref().expect("the tests carry no list"),
            })
            .collect()
    }

    /// Encode one owned row, as the tests around the format spell it.
    fn encode(entity: u32, fields: &[RecordField], out: &mut Vec<u8>) -> Result<(), RecordError> {
        encode_row(entity, &borrowed(fields), out)
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
        encode(42, &fields, &mut buf).expect("encode");
        assert_eq!(decode_row(payload(&buf), 42).expect("decode"), fields);
    }

    /// A row is its fields and nothing else: the entity names it in a refusal and is not encoded,
    /// so the same bytes decode under any entity. What says whose row it is is the block header.
    #[test]
    fn a_row_carries_no_entity_of_its_own() {
        let fields = vec![field(0, RecordValue::U8(1))];
        let mut buf = Vec::new();
        encode(7, &fields, &mut buf).expect("encode");
        let mut other = Vec::new();
        encode(8, &fields, &mut other).expect("encode");
        assert_eq!(buf, other);
        assert_eq!(buf.len(), 5, "a length byte, then tag, kind and value");
    }

    /// The length that heads a row is a varint, so it is one byte on a row under 128 bytes and
    /// two under 16 KiB. That is what it saves against the four uncompressed bytes a rank-indexed
    /// offset costs in the directory.
    #[test]
    fn a_row_states_its_length_as_a_varint() {
        for (chars, width) in [(1usize, 1usize), (100, 1), (200, 2), (20_000, 3)] {
            let fields = vec![field(0, RecordValue::Utf8("x".repeat(chars)))];
            let mut buf = Vec::new();
            encode(1, &fields, &mut buf).expect("encode");
            // tag, kind and the utf8 length prefix are seven bytes around the characters.
            assert_eq!(buf.len(), width + 7 + chars, "{chars} characters");
            assert_eq!(decode_row(payload(&buf), 1).expect("decode"), fields);
        }
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
            encode_block_header(13, &entities, &mut out).expect("header");
            let header = decode_block_header(&out, 0).expect("decode");
            assert_eq!(header.row_count, entities.len());
            assert_eq!(header.first_rank, 13);
            assert_eq!(header.first_entity, entities[0]);
            assert_eq!(
                header.rows_at,
                out.len(),
                "the gaps end where the rows begin"
            );
            // The gaps walked through a scan that never reads a row, which is how a reader
            // recovers an entity without decoding one.
            let mut scan = BlockScan::start(&header);
            for expect in &entities {
                assert_eq!(scan.entity, *expect);
                scan.local += 1;
                if scan.local < header.row_count {
                    let gap =
                        take_varint(&out[..header.rows_at], &mut scan.gaps_at).expect("a gap");
                    scan.entity += gap + 1;
                }
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
            let err =
                encode_block_header(0, &entities, &mut out).expect_err("entities ascend strictly");
            assert!(err.to_string().contains("ascending"), "{err}");
        }
    }

    /// An empty block is never written, and the reader refuses one that claims to be.
    #[test]
    fn an_empty_block_refuses_at_both_ends() {
        let mut out = Vec::new();
        assert!(encode_block_header(0, &[], &mut out).is_err());
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
        encode_block_header(0, &[1, 2, 3], &mut out).expect("header");
        out[0..4].copy_from_slice(&40u32.to_le_bytes());
        let err = decode_block_header(&out, 0).expect_err("the gaps are short");
        assert!(err.to_string().contains("varint"), "{err}");
    }

    /// A truncated payload refuses: the field walk is bounded by the extent the row's own length
    /// gave it.
    #[test]
    fn a_truncated_row_refuses() {
        let mut buf = Vec::new();
        encode(3, &[field(0, RecordValue::Utf8("hello".into()))], &mut buf).expect("encode");
        let buf = payload(&buf).to_vec();
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
        let err = encode(1, &fields, &mut buf).expect_err("duplicate tags refuse");
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

    /// A list cannot be written until epic 3's multi surface lands (records §5): the encoder takes
    /// borrowed values and [`RecordValueRef`] has no list arm, so a producer holding a decoded
    /// list cannot hand one over.
    #[test]
    fn a_list_cannot_be_handed_to_the_encoder() {
        let list = RecordValue::List(vec![RecordValue::U8(1)]);
        assert!(list.as_ref().is_none(), "a list does not borrow");
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
