//! **What does each part of the record blob's row framing cost, compressed?**
//!
//! `records-and-search.md` §3 states the framing's share of `blocks.bin` and
//! [decision 0141](../../../../docs/decisions/0141-the-record-blob-states-identity-once-per-block.md)
//! rests on what each part of it costs separately. Neither is answerable from the raw byte counts:
//! the row header is four bytes of ascending entity id, which zstd predicts almost perfectly, so
//! its raw share and its compressed share are different numbers by an order of magnitude. This
//! re-encodes every block of a built blob under each candidate framing, at the shipped compression
//! level and the shipped block cut, and reports what each one actually costs.
//!
//! **The control is exact.** `as shipped` re-encodes through the format's own
//! `encode_block_header` and `encode_row`, so it reproduces `blocks.bin` byte for byte on a blob
//! this reader opens. A candidate is therefore comparable with the shipped file rather than with
//! another transcription of it.
//!
//! ```text
//! cargo run --release -p tessera-bench --bin blob_framing_probe -- \
//!     <bundle>/v00000/partitions/default/attrs/record [max blocks]
//! ```
//!
//! A block limit samples the first *n* blocks, which is a prefix of entity space rather than a
//! random sample: the framing ratio is stable across a corpus and the value bytes are not, so read
//! the framing columns from a sample and the absolute B/row from a whole run.
//!
//! A third argument times that many random single-row reads through the shipped reader, which is
//! what prices the gap walk a row's identity now costs: reaching row *k* of a block reads one
//! varint per row before it.

use std::path::PathBuf;

use tessera_filter::{
    encode_block_header, Access, RecordBlob, RecordField, RecordValue,
};

/// The writer's level (`tessera_filter_write`'s `ZSTD_LEVEL`), so a candidate is priced at the
/// operating point the shipped blob was written at.
const LEVEL: i32 = 3;

fn varint(mut v: u64, out: &mut Vec<u8>) {
    loop {
        let b = (v & 0x7f) as u8;
        v >>= 7;
        if v == 0 {
            out.push(b);
            return;
        }
        out.push(b | 0x80);
    }
}

fn value_bytes(v: &RecordValue, out: &mut Vec<u8>, len_varint: bool, with_len: bool) {
    match v {
        RecordValue::Bool(b) => out.push(u8::from(*b)),
        RecordValue::U8(x) => out.push(*x),
        RecordValue::U16(x) => out.extend_from_slice(&x.to_le_bytes()),
        RecordValue::U32(x) => out.extend_from_slice(&x.to_le_bytes()),
        RecordValue::U64(x) => out.extend_from_slice(&x.to_le_bytes()),
        RecordValue::I8(x) => out.extend_from_slice(&x.to_le_bytes()),
        RecordValue::I16(x) => out.extend_from_slice(&x.to_le_bytes()),
        RecordValue::I32(x) => out.extend_from_slice(&x.to_le_bytes()),
        RecordValue::I64(x) => out.extend_from_slice(&x.to_le_bytes()),
        RecordValue::F32(x) => out.extend_from_slice(&x.to_le_bytes()),
        RecordValue::F64(x) => out.extend_from_slice(&x.to_le_bytes()),
        RecordValue::TimestampUs(x) => out.extend_from_slice(&x.to_le_bytes()),
        RecordValue::Utf8(s) => {
            if with_len {
                if len_varint {
                    varint(s.len() as u64, out);
                } else {
                    out.extend_from_slice(&(s.len() as u32).to_le_bytes());
                }
            }
            out.extend_from_slice(s.as_bytes());
        }
        RecordValue::List(_) => unreachable!("no writer emits lists (records §5)"),
    }
}

fn kind_of(v: &RecordValue) -> u8 {
    match v {
        RecordValue::Bool(_) => 0,
        RecordValue::U8(_) => 1,
        RecordValue::U16(_) => 2,
        RecordValue::U32(_) => 3,
        RecordValue::U64(_) => 4,
        RecordValue::I8(_) => 5,
        RecordValue::I16(_) => 6,
        RecordValue::I32(_) => 7,
        RecordValue::I64(_) => 8,
        RecordValue::F32(_) => 9,
        RecordValue::F64(_) => 10,
        RecordValue::TimestampUs(_) => 11,
        RecordValue::Utf8(_) => 12,
        RecordValue::List(_) => 13,
    }
}

/// Where a row's identity is written, if anywhere.
#[derive(Clone, Copy, PartialEq)]
enum Id {
    /// Nowhere: the block says nothing about which entity a row belongs to.
    None,
    /// The shipped block header: row count, first rank, first entity, extent digest, then one
    /// varint gap per row after the first.
    BlockHeader,
    /// Four bytes at the head of every row, which is what format 8 wrote.
    PerRowU32,
    /// The entity's low eight bits at the head of every row.
    PerRowDigit,
    /// A varint gap at the head of every row, interleaved with the values rather than columnar.
    PerRowGap,
}

/// How a row's extent is stated, if at all. `None` leaves it to the directory's offsets, which is
/// what the shipped format does.
#[derive(Clone, Copy, PartialEq)]
enum Len {
    None,
    PerRowU32,
    /// Columnar, at the head of the block, one varint per row.
    BlockHeader,
}

/// How a field states what it is. `None` strips the self-description entirely, which is not a
/// candidate — it is the floor the other columns are read against.
#[derive(Clone, Copy, PartialEq)]
enum Field {
    TagKind,
    VarintTagKind,
    ValuesOnly,
}

struct Scheme {
    name: &'static str,
    id: Id,
    len: Len,
    field: Field,
    /// Whether a `utf8` value's length prefix is a varint rather than four bytes.
    varint_str: bool,
}

fn payload_of(s: &Scheme, fields: &[RecordField]) -> Vec<u8> {
    let mut out = Vec::new();
    for f in fields {
        match s.field {
            Field::TagKind => {
                out.extend_from_slice(&f.tag.to_le_bytes());
                out.push(kind_of(&f.value));
            }
            Field::VarintTagKind => {
                varint(u64::from(f.tag), &mut out);
                out.push(kind_of(&f.value));
            }
            Field::ValuesOnly => {}
        }
        value_bytes(
            &f.value,
            &mut out,
            s.varint_str,
            s.field != Field::ValuesOnly,
        );
    }
    out
}

fn encode_block(s: &Scheme, rows: &[(u32, Vec<RecordField>)], first_rank: u32) -> Vec<u8> {
    let payloads: Vec<Vec<u8>> = rows.iter().map(|(_, f)| payload_of(s, f)).collect();
    let mut offsets = Vec::with_capacity(rows.len());
    let mut at = 0u32;
    for p in &payloads {
        offsets.push(at);
        at += p.len() as u32;
    }

    let mut out = Vec::new();
    if s.id == Id::BlockHeader {
        // The shipped header, through the format's own writer, so `as shipped` is exact.
        let entities: Vec<u32> = rows.iter().map(|(e, _)| *e).collect();
        encode_block_header(first_rank, &entities, &mut out).expect("ascending entities");
    }
    if s.len == Len::BlockHeader {
        for p in &payloads {
            varint(p.len() as u64, &mut out);
        }
    }
    let first = rows[0].0;
    let mut previous = first;
    for ((entity, _), payload) in rows.iter().zip(&payloads) {
        match s.id {
            Id::PerRowU32 => out.extend_from_slice(&entity.to_le_bytes()),
            Id::PerRowDigit => out.push(*entity as u8),
            Id::PerRowGap => {
                varint(u64::from(entity - previous), &mut out);
                previous = *entity;
            }
            Id::None | Id::BlockHeader => {}
        }
        match s.len {
            Len::PerRowU32 => out.extend_from_slice(&(payload.len() as u32).to_le_bytes()),
            Len::None | Len::BlockHeader => {}
        }
        out.extend_from_slice(payload);
    }
    out
}

fn scheme(name: &'static str, id: Id, len: Len, field: Field, varint_str: bool) -> Scheme {
    Scheme {
        name,
        id,
        len,
        field,
        varint_str,
    }
}

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let mut args = std::env::args().skip(1);
    let dir = PathBuf::from(
        args.next()
            .ok_or("usage: blob_framing_probe <attrs/record dir> [max blocks]")?,
    );
    let max_blocks: usize = args
        .next()
        .and_then(|v| v.parse().ok())
        .unwrap_or(usize::MAX);
    let reads: usize = args.next().and_then(|v| v.parse().ok()).unwrap_or(0);

    let blob = RecordBlob::open_dir(&dir, Access::Read)?;
    let blocks = blob.block_count().min(max_blocks);
    let shipped = std::fs::metadata(dir.join("blocks.bin"))?.len();

    let schemes = [
        scheme("as shipped", Id::BlockHeader, Len::None, Field::TagKind, false),
        scheme("+ per-row length", Id::BlockHeader, Len::PerRowU32, Field::TagKind, false),
        scheme("+ columnar lengths", Id::BlockHeader, Len::BlockHeader, Field::TagKind, false),
        scheme("format 8", Id::PerRowU32, Len::PerRowU32, Field::TagKind, false),
        scheme("f8, check digit", Id::PerRowDigit, Len::PerRowU32, Field::TagKind, false),
        scheme("f8, per-row gap", Id::PerRowGap, Len::PerRowU32, Field::TagKind, false),
        scheme("f8, no identity", Id::None, Len::PerRowU32, Field::TagKind, false),
        scheme("no identity", Id::None, Len::None, Field::TagKind, false),
        scheme("+ varint tag, strlen", Id::BlockHeader, Len::None, Field::VarintTagKind, true),
        scheme("values only", Id::None, Len::None, Field::ValuesOnly, false),
    ];
    let mut compressed = vec![0u64; schemes.len()];
    let mut raw = vec![0u64; schemes.len()];
    let mut rows_total = 0u64;
    let mut chars = 0u64;

    for b in 0..blocks {
        let mut cursor = blob.rows_cursor_over(b, b + 1);
        let mut rows = Vec::new();
        while let Some(r) = cursor.next_row()? {
            rows.push(r);
        }
        if rows.is_empty() {
            continue;
        }
        let first_rank = rows_total as u32;
        rows_total += rows.len() as u64;
        for (_, fields) in &rows {
            for f in fields {
                if let RecordValue::Utf8(s) = &f.value {
                    chars += s.len() as u64;
                }
            }
        }
        for (i, s) in schemes.iter().enumerate() {
            let bytes = encode_block(s, &rows, first_rank);
            raw[i] += bytes.len() as u64;
            compressed[i] += zstd::bulk::compress(&bytes, LEVEL)?.len() as u64;
        }
    }

    if reads > 0 {
        // A cheap deterministic spread over the has-row bitmap: every (cardinality / reads)-th
        // member, which touches a different block almost every time.
        let entities: Vec<u32> = blob.hasrow().iter().collect();
        let stride = entities.len().checked_div(reads).unwrap_or(1).max(1);
        let sample: Vec<u32> = entities.iter().step_by(stride).copied().take(reads).collect();
        let start = std::time::Instant::now();
        let mut found = 0u64;
        for entity in &sample {
            if blob.fields_of(*entity)?.is_some() {
                found += 1;
            }
        }
        let elapsed = start.elapsed();
        println!(
            "\n{} random single-row read(s), {found} with a row: {:.1} µs/read",
            sample.len(),
            elapsed.as_secs_f64() * 1e6 / sample.len() as f64
        );
    }

    let items = rows_total as f64;
    println!("\n{}", dir.display());
    println!(
        "blocks {blocks}, rows {rows_total}, utf8 chars {chars} ({:.3} B/row), \
         shipped blocks.bin {shipped}",
        chars as f64 / items
    );
    println!(
        "{:<24} {:>14} {:>14} {:>11} {:>11}",
        "framing", "compressed", "raw", "comp B/row", "vs shipped"
    );
    let base = compressed[0] as f64;
    for (i, s) in schemes.iter().enumerate() {
        println!(
            "{:<24} {:>14} {:>14} {:>11.3} {:>10.1}%",
            s.name,
            compressed[i],
            raw[i],
            compressed[i] as f64 / items,
            100.0 * (compressed[i] as f64 - base) / base
        );
    }
    if blocks == blob.block_count() && compressed[0] != shipped {
        println!(
            "\nWARNING: the control re-encoded to {} bytes where blocks.bin holds {shipped}; the \
             probe and the writer disagree about the format and no row below is comparable.",
            compressed[0]
        );
    }
    Ok(())
}
