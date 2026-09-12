//! `columns.arrow` written **in place**: the file is reserved at its final size and each column's
//! values are filled at their own offset as the rows are produced.
//!
//! ## Why a second route into the same bytes
//!
//! [`crate::write::write_columns`] takes whole columns by value and hands them to Arrow's
//! `FileWriter`. That is the right shape for a producer that holds its columns — the flush, the
//! merge, a test — and the wrong one for the build, whose row order arrives one Morton bucket at
//! a time and whose columns at rung 6 are 42 GB. Materialising them to hand over is the row-sized
//! anonymous structure the bounded-assembly rule forbids, and materialising them into mapped
//! scratch is the same bytes written to disk twice.
//!
//! The layout of an Arrow IPC file carrying **one** record batch of non-nullable fixed-width
//! columns is a function of the schema and the row count alone: every length in the IPC metadata
//! is a fixed-width integer, so the schema message, the record-batch metadata, each buffer's
//! offset in the body and the footer are all known before the first row exists. So the file can
//! be laid out first and filled afterwards, in any order, and the bytes are the bytes the other
//! route writes.
//!
//! ## How the layout is learnt
//!
//! Not by rebuilding Arrow's framing by hand — a second encoder is how two writers come to
//! disagree about a format (write-path §7). The framing is produced by **Arrow's own
//! `FileWriter`**, run once over a batch of the real schema and the real row count whose values
//! buffers are untouched anonymous mappings, into a sink that keeps every byte it is handed
//! **except** the values buffers themselves, which it records the position and length of. An
//! untouched mapping costs address space and no resident page, and the writer never reads one: a
//! null-free array's null count is a stored number, and no other field of the metadata depends on
//! a value. What comes back is the file's length, the framing bytes at their offsets, and where
//! each column's values belong.
//!
//! `a_column_file_filled_in_place_is_byte_identical_to_one_written_whole` in
//! `tests/segment_roundtrip.rs` holds the two routes to the same bytes over every declarable
//! width, over a column set with no scalars, and over no rows at all. What it compares is two
//! routes through **one** arrow, so it says nothing about a future one: `arrow` is pinned exactly
//! in the workspace manifest for that reason, and the [`Recorder`]'s refusal of an unrecognised
//! write above [`MAX_FRAMING_WRITE`] is the only thing that would catch a version whose writer
//! encoded a batch contiguously instead of buffer by buffer.

use std::fs::File;
use std::io::{self, Write};
use std::os::unix::fs::FileExt;
use std::path::Path;
use std::sync::Arc;

use arrow::array::{ArrayRef, BooleanArray, UInt32Array, UInt64Array};
use arrow::buffer::{BooleanBuffer, Buffer, ScalarBuffer};
use arrow::datatypes::{DataType, Field, Schema};
use arrow::ipc::writer::FileWriter;
use arrow::record_batch::RecordBatch;

use tessera_spatial::tiler::ScalarType;

/// The largest single write the framing keeps as bytes. Above it a write must be uniform — which
/// is what the validity bitmaps are — and is kept as a fill; anything else that big is a values
/// buffer the sink failed to recognise, which means arrow's writer has changed shape under us and
/// the layout below would be wrong. A refusal, never a silent 42 GB `Vec`.
const MAX_FRAMING_WRITE: usize = 1 << 16;

/// How much of a [`Fill`] is written per call.
const FILL_CHUNK: usize = 1 << 20;

/// A run of the framing that is one byte repeated — **the validity bitmaps**. Every column of
/// `columns.arrow` is non-nullable (contracts R4) and arrow writes a validity buffer for it all the
/// same, `rows / 8` bytes of `0xFF` saying nothing
/// (`docs/evidence/memos/2026-09-11-arrow-all-ones-validity-buffers.md`). At rung 6 that is 437 MB
/// a column.
///
/// **A fill saves the file, not the process.** Held as three numbers, the bitmap costs the plan
/// nothing to carry and the write nothing to emit. It is still allocated once, by arrow, inside
/// [`ColumnsPlan::new`]: the IPC writer builds every buffer of the batch before it writes any of
/// them, so one all-ones bitmap per column is alive together during the layout pass. The build's
/// residency model charges them (`residency::entity_order_residency`, the assembly phase); this
/// type does not get to pretend they are free.
struct Fill {
    at: u64,
    len: u64,
    byte: u8,
}

/// Where every byte of a `columns.arrow` goes, for a given schema and row count.
pub struct ColumnsPlan {
    /// The file's final length.
    len: u64,
    /// Everything that is not a values buffer — magic, messages, padding, footer — at its offset.
    framing: Vec<(u64, Vec<u8>)>,
    /// The long uniform runs of the same, kept as three numbers rather than as their bytes.
    fills: Vec<Fill>,
    /// Each column's values buffer: its offset in the file and its length. A column of no rows
    /// has a zero-length buffer and Arrow writes nothing for it.
    values: Vec<(u64, u64)>,
    /// One entry per column: the width of a value in its buffer, or `None` for `bool`, whose
    /// buffer is one bit a row.
    widths: Vec<Option<usize>>,
    rows: usize,
}

impl ColumnsPlan {
    /// The plan for a segment of `rows` rows carrying the two fixed columns of contracts §2.6 and
    /// then `scalars` in declared order — the same schema, in the same order, that
    /// [`crate::write::write_columns`] builds from the same inputs.
    pub fn new(scalars: &[(String, ScalarType)], rows: usize) -> io::Result<ColumnsPlan> {
        let mut fields = vec![
            Field::new("tessera_id", DataType::UInt64, false),
            Field::new("residual", DataType::UInt32, false),
        ];
        let mut widths: Vec<Option<usize>> = vec![Some(8), Some(4)];
        for (name, ty) in scalars {
            fields.push(Field::new(name, crate::write::arrow_type_of(*ty), false));
            widths.push(value_width(*ty)?);
        }
        let schema = Arc::new(Schema::new(fields));

        // One untouched mapping per column, long enough for its buffer. `MmapMut` is zeroed and
        // demand-paged: nothing below reads it, so it costs address space alone.
        let mut regions: Vec<memmap2::MmapMut> = Vec::with_capacity(widths.len());
        for width in &widths {
            let bytes = buffer_bytes(*width, rows)?;
            regions.push(
                memmap2::MmapOptions::new()
                    .len(bytes.max(1))
                    .map_anon()
                    .map_err(|e| {
                        io::Error::new(
                            e.kind(),
                            format!("columns.arrow: reserving {bytes} bytes to plan the layout: {e}"),
                        )
                    })?,
            );
        }
        // The span is the buffer's own byte length, not the mapping's: two mappings can be
        // adjacent, and a span rounded up would claim a neighbour's first byte.
        let mut spans: Vec<(usize, usize)> = Vec::with_capacity(regions.len());
        for (index, region) in regions.iter().enumerate() {
            spans.push((region.as_ptr() as usize, buffer_bytes(widths[index], rows)?));
        }

        let mut columns: Vec<ArrayRef> = Vec::with_capacity(widths.len());
        for (index, width) in widths.iter().enumerate() {
            // SAFETY: the mapping is alive for the length of this function — `regions` is dropped
            // below, after the writer has finished — and the buffer never outlives it.
            let bytes = buffer_bytes(*width, rows)?;
            let buffer = unsafe {
                Buffer::from_custom_allocation(
                    std::ptr::NonNull::new(regions[index].as_ptr() as *mut u8)
                        .expect("a mapping's pointer is not null"),
                    bytes,
                    Arc::new(()),
                )
            };
            columns.push(array_of(schema.field(index).data_type(), buffer, rows)?);
        }
        let batch = RecordBatch::try_new(schema.clone(), columns)
            .map_err(|e| io::Error::new(io::ErrorKind::InvalidData, e.to_string()))?;

        let mut sink = Recorder {
            at: 0,
            spans,
            framing: Vec::new(),
            fills: Vec::new(),
            values: vec![(0, 0); widths.len()],
            failed: None,
        };
        {
            let mut writer = FileWriter::try_new(&mut sink, &schema)
                .map_err(|e| io::Error::new(io::ErrorKind::InvalidData, e.to_string()))?;
            writer
                .write(&batch)
                .map_err(|e| io::Error::new(io::ErrorKind::InvalidData, e.to_string()))?;
            writer
                .finish()
                .map_err(|e| io::Error::new(io::ErrorKind::InvalidData, e.to_string()))?;
        }
        drop(batch);
        drop(regions);
        if let Some(message) = sink.failed {
            return Err(io::Error::new(io::ErrorKind::Unsupported, message));
        }
        Ok(ColumnsPlan {
            len: sink.at,
            framing: sink.framing,
            fills: sink.fills,
            values: sink.values,
            widths,
            rows,
        })
    }

    /// The file's final length in bytes.
    pub fn len(&self) -> u64 {
        self.len
    }

    pub fn is_empty(&self) -> bool {
        self.len == 0
    }
}

/// What the layout pass writes into: every byte but the values buffers is kept, and each values
/// buffer is recognised by the mapping its bytes come from.
struct Recorder {
    at: u64,
    spans: Vec<(usize, usize)>,
    framing: Vec<(u64, Vec<u8>)>,
    fills: Vec<Fill>,
    values: Vec<(u64, u64)>,
    failed: Option<String>,
}

impl Write for Recorder {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        let start = buf.as_ptr() as usize;
        let owner = self
            .spans
            .iter()
            .position(|&(base, len)| len > 0 && start >= base && start + buf.len() <= base + len);
        match owner {
            Some(column) => self.values[column] = (self.at, buf.len() as u64),
            None if buf.len() <= MAX_FRAMING_WRITE => self.framing.push((self.at, buf.to_vec())),
            None if buf.iter().all(|byte| *byte == buf[0]) => self.fills.push(Fill {
                at: self.at,
                len: buf.len() as u64,
                byte: buf[0],
            }),
            None => {
                self.failed.get_or_insert_with(|| {
                    format!(
                        "columns.arrow: arrow's file writer handed {} bytes that are neither one \
                         of the batch's values buffers nor a uniform run, so the in-place layout \
                         cannot say where they belong. The writer's shape has changed and this \
                         module has to follow it",
                        buf.len()
                    )
                });
            }
        }
        self.at += buf.len() as u64;
        Ok(buf.len())
    }

    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}

/// A `columns.arrow` reserved at its final size, filled column by column.
pub struct ColumnsFile {
    file: File,
    plan: ColumnsPlan,
}

impl ColumnsFile {
    /// Reserve the file and write its framing. Every values buffer is zero until it is filled, and
    /// an absent render slot's value **is** the zero a fresh file reads as (decision 0064), so a
    /// column the caller only writes the present rows of comes out exactly as the whole-column
    /// route's does.
    pub fn create(path: &Path, plan: ColumnsPlan) -> io::Result<ColumnsFile> {
        let file = File::create(path)?;
        file.set_len(plan.len)?;
        for (at, bytes) in &plan.framing {
            file.write_all_at(bytes, *at)?;
        }
        // A fill is written a chunk at a time, so a 437 MB validity bitmap costs a mebibyte of
        // transient and not its own length. A run of zeros is what `set_len` already laid down.
        let mut chunk: Vec<u8> = Vec::new();
        for fill in &plan.fills {
            if fill.byte == 0 {
                continue;
            }
            chunk.clear();
            chunk.resize(FILL_CHUNK.min(fill.len as usize), fill.byte);
            let mut written = 0u64;
            while written < fill.len {
                let take = chunk.len().min((fill.len - written) as usize);
                file.write_all_at(&chunk[..take], fill.at + written)?;
                written += take as u64;
            }
        }
        Ok(ColumnsFile { file, plan })
    }

    /// How many rows the plan was made for.
    pub fn rows(&self) -> usize {
        self.plan.rows
    }

    /// Write `bytes` into column `column`'s values buffer, `at` bytes in.
    ///
    /// For every column but a `bool` one, `at` is `row × width`. A `bool` column's buffer is one
    /// bit a row, least significant bit first, so `at` is `row / 8` and the caller's run must
    /// start on a byte boundary — which is what a bucket boundary that is a multiple of 64 gives
    /// it.
    pub fn put(&self, column: usize, at: u64, bytes: &[u8]) -> io::Result<()> {
        let (offset, len) = *self.plan.values.get(column).ok_or_else(|| {
            io::Error::new(
                io::ErrorKind::InvalidInput,
                format!(
                    "columns.arrow: column {column} of {}",
                    self.plan.values.len()
                ),
            )
        })?;
        let end = at
            .checked_add(bytes.len() as u64)
            .filter(|end| *end <= len)
            .ok_or_else(|| {
                io::Error::new(
                    io::ErrorKind::InvalidInput,
                    format!(
                        "columns.arrow: column {column} is {len} bytes and a run of {} at {at} \
                         would pass its end",
                        bytes.len()
                    ),
                )
            })?;
        debug_assert!(end <= len);
        self.file.write_all_at(bytes, offset + at)
    }

    /// A value's width in column `column`'s buffer, or `None` for the bit-packed `bool`.
    pub fn width(&self, column: usize) -> Option<usize> {
        self.plan.widths.get(column).copied().flatten()
    }

    pub fn finish(self) -> io::Result<()> {
        self.file.sync_all()
    }
}

/// A column's values buffer in bytes: `rows` values, or `rows` bits for a `bool`.
fn buffer_bytes(width: Option<usize>, rows: usize) -> io::Result<usize> {
    match width {
        Some(width) => rows.checked_mul(width).ok_or_else(|| {
            io::Error::new(
                io::ErrorKind::InvalidInput,
                format!("columns.arrow: {rows} rows of {width} bytes overflow usize"),
            )
        }),
        None => Ok(rows.div_ceil(8)),
    }
}

/// The width one value of `ty` occupies in its Arrow values buffer — `None` for the bit-packed
/// `bool`, and a refusal for the string family, which `render` is declined for at the declaration
/// and which therefore never reaches a hot column (per-point-attributes §4.3).
fn value_width(ty: ScalarType) -> io::Result<Option<usize>> {
    Ok(match ty {
        ScalarType::Bool => None,
        ScalarType::U8 | ScalarType::I8 => Some(1),
        ScalarType::U16 | ScalarType::I16 => Some(2),
        ScalarType::U32 | ScalarType::I32 | ScalarType::F32 => Some(4),
        ScalarType::U64 | ScalarType::I64 | ScalarType::F64 | ScalarType::TimestampUs => Some(8),
        ScalarType::Utf8 | ScalarType::Keyword | ScalarType::Text => {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                format!(
                    "columns.arrow: a render column is a fixed-width slot in every row and \
                     {ty:?} is not one"
                ),
            ))
        }
    })
}

/// The null-free array the layout pass puts in front of Arrow's writer. Its buffer is never read;
/// what it decides is the metadata's shape.
fn array_of(ty: &DataType, buffer: Buffer, rows: usize) -> io::Result<ArrayRef> {
    macro_rules! flat {
        ($arr:ty, $native:ty) => {
            Arc::new(<$arr>::new(
                ScalarBuffer::<$native>::new(buffer, 0, rows),
                None,
            )) as ArrayRef
        };
    }
    Ok(match ty {
        DataType::Boolean => Arc::new(BooleanArray::new(BooleanBuffer::new(buffer, 0, rows), None)),
        DataType::UInt8 => flat!(arrow::array::UInt8Array, u8),
        DataType::UInt16 => flat!(arrow::array::UInt16Array, u16),
        DataType::UInt32 => flat!(UInt32Array, u32),
        DataType::UInt64 => flat!(UInt64Array, u64),
        DataType::Int8 => flat!(arrow::array::Int8Array, i8),
        DataType::Int16 => flat!(arrow::array::Int16Array, i16),
        DataType::Int32 => flat!(arrow::array::Int32Array, i32),
        DataType::Int64 => flat!(arrow::array::Int64Array, i64),
        DataType::Float32 => flat!(arrow::array::Float32Array, f32),
        DataType::Float64 => flat!(arrow::array::Float64Array, f64),
        DataType::Timestamp(arrow::datatypes::TimeUnit::Microsecond, None) => {
            flat!(arrow::array::TimestampMicrosecondArray, i64)
        }
        other => {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                format!("columns.arrow: {other:?} is not a hot column's type"),
            ))
        }
    })
}
