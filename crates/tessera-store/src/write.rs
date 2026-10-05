//! Segment file writers: `columns.arrow`, `morton.u32`, `permutation.bin` (contracts §2.1/§2.6).
//! Byte formats here are fixed by the contracts spec — do not vary them.
//!
//! ## One writer, several producers
//!
//! [`SegmentWriter`] takes segment rows one at a time, in row order, and holds none of them: each
//! column's bytes go straight to a spool file beside the output, and the single record batch
//! contracts §2.6 requires is assembled at [`SegmentWriter::finish`] with the spools
//! memory-mapped as its values buffers. That is `PostingsSpool`'s discipline
//! (`tessera_authz::postings`), for the same reason — the compaction fold's pass 1 streams the
//! whole corpus through here and may not materialise it (compaction §3), and merge's measured
//! **4.4–4.9× peak over its inputs' bytes** (`probes/2026-08-04-maintenance-memory/`) is what
//! forced `max_merged_segment_bytes` to stay where decision 0049 left it.
//!
//! Its two producers are [`write_segment`], for an already-sorted in-memory batch (flush and the
//! build's tiler), and [`crate::merge::execute_merge`]'s k-way merge, for rows streamed off mapped
//! inputs. *A second writer that knows the layout is how two come to disagree about a format*
//! (write-path §7), so the merge feeds this one rather than growing its own — and the two paths
//! then produce byte-identical files for equal rows by construction rather than by argument
//! (`merge_execution::the_k_way_merge_emits_exactly_what_a_concatenate_and_sort_would`).
//!
//! The batch build lays `columns.arrow` out in place instead ([`crate::columns`]), and its bytes
//! are held equal to this writer's for the same rows
//! (`segment_roundtrip::a_column_file_filled_in_place_is_byte_identical_to_one_written_whole`).

use std::fs::File;
use std::io::{self, BufWriter, Write};
use std::path::{Path, PathBuf};
use std::ptr::NonNull;
use std::sync::Arc;

use arrow::array::{
    ArrayRef, BooleanArray, Float32Array, Float64Array, Int16Array, Int32Array,
    Int64Array, Int8Array, StringArray, TimestampMicrosecondArray, UInt16Array, UInt32Array,
    UInt64Array, UInt8Array,
};
use arrow::buffer::{Buffer, OffsetBuffer, ScalarBuffer};
use arrow::datatypes::{ArrowNativeType, DataType, Field, Schema, TimeUnit};
use arrow::ipc::writer::FileWriter;
use arrow::record_batch::RecordBatch;

use tessera_spatial::split32;
use tessera_spatial::tiler::{ScalarType, ScalarValue, TilerItem};
use tessera_types::{EntityId, TesseraId};

use crate::permutation::{
    pages_for, payload_start, PAGE_ABSENT, PAGE_BYTES, PAGE_ENTRIES, PAGE_SHIFT,
};

const PERMUTATION_MAGIC: &[u8; 4] = b"TSPM";
/// Version 2 is the paged form; version 1 was the flat array it replaced, and a reader refuses it
/// on this field alone (`crate::permutation`).
const PERMUTATION_VERSION: u16 = 2;
const PERMUTATION_ABSENT: u32 = 0xFFFF_FFFF;

/// The header's width, from the one module that defines the layout.
const PERMUTATION_HEADER_BYTES: usize = crate::permutation::HEADER_LEN;

/// The largest satisfiable `permutation.bin` bound: entity ids must fit `u32` in
/// `bundle_format = 1` (R1), so the highest addressable id is `2^32 - 1` and the bound — one past
/// it — is `2^32`. A bound above this names no entity that could ever occupy a slot.
const PERMUTATION_MAX_BOUND: u64 = 1 << 32;

/// Write `columns.arrow` (Arrow IPC file format, one record batch, uncompressed buffers) and
/// `morton.u32` (raw little-endian `u32` codes, no header) into `dir`.
///
/// `items` and `codes` must already be in row order (i.e. the output of
/// [`tessera_spatial::tiler::sort_batch`]) and the same length; row *i*'s Morton code is
/// `codes[i]`. `scalar_schema` declares the name and Arrow type of each item's leading
/// `scalars`, in the order they appear in `TilerItem::scalars`, and `indexed` those of the values
/// after them, which the bands copy and `columns.arrow` does not hold.
///
/// **One of [`SegmentWriter`]'s producers, not a second writer** — see the module doc. The caller
/// already holds every row, so nothing here is streamed *in*; what the delegation buys is that
/// this path and the merge's cannot drift apart in layout.
pub fn write_segment(
    dir: &Path,
    items: &[TilerItem],
    codes: &[u32],
    scalar_schema: &[(String, ScalarType)],
    indexed: &[(String, ScalarType)],
) -> io::Result<()> {
    if items.len() != codes.len() {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            format!(
                "write_segment: items.len() ({}) != codes.len() ({})",
                items.len(),
                codes.len()
            ),
        ));
    }

    let mut writer = SegmentWriter::create(dir, scalar_schema, indexed)?;
    let drawn = scalar_schema.len();
    for (item, code) in items.iter().zip(codes) {
        let (scalars, after) = item.scalars.split_at(drawn.min(item.scalars.len()));
        let value = |k: usize| {
            after.get(k).cloned().ok_or_else(|| {
                io::Error::new(
                    io::ErrorKind::InvalidInput,
                    format!(
                        "write_segment: tessera_id {} has no value for indexed column {k}",
                        item.tessera_id.raw()
                    ),
                )
            })
        };
        writer.append(SegmentRow {
            tessera_id: item.tessera_id,
            morton: *code,
            // The residual is the low half of the same `split32` whose high half the tiler
            // returned as this row's code, so the stored pair is one splitting of one position
            // rather than two derivations that could disagree.
            residual: split32(item.qx, item.qy).1,
            scalars,
            indexed: &value,
        })?;
    }
    writer.finish()?;
    Ok(())
}

/// Writes one segment's `cuts.u32` and `cell-codes.u32` from the Morton codes of its rows,
/// arriving in row order.
///
/// A cell's start and its code are written the first time a code is seen, so `cuts.u32` is the
/// run-length index of `morton.u32` and `cell-codes.u32` the code of each run:
/// `crate::read::CutIndex` and [`crate::bands`] describe what each is for. Nothing is held but the
/// previous code and two `BufWriter`s.
///
/// **One writer for every producer.** The build's bounded assembly emits `morton.u32` positionally
/// and [`SegmentWriter`] emits it row by row, so the two paths have no code in common but this; a
/// build that indexed its segment differently from a flush would serve two different selections
/// from one bundle.
///
/// **It counts nothing.** The cell count is the length of either file, and the build's
/// `OccupancyRun` derives the same figure from the same codes; a third counter would be a third
/// thing that could disagree.
pub struct CutWriter {
    out: BufWriter<File>,
    codes: BufWriter<File>,
    last: Option<u32>,
    row: u32,
}

impl CutWriter {
    /// Create `cuts.u32` and `cell-codes.u32` in `dir`, which must exist.
    pub fn create(dir: &Path) -> io::Result<Self> {
        Ok(CutWriter {
            out: BufWriter::new(File::create(dir.join(crate::read::CutIndex::FILE))?),
            codes: BufWriter::new(File::create(dir.join(crate::bands::CELL_CODES_FILE))?),
            last: None,
            row: 0,
        })
    }

    /// Take the next row's Morton code. Codes must arrive in the row order the segment is written
    /// in, which is ascending; a repeated code continues the cell it started.
    pub fn push(&mut self, morton: u32) -> io::Result<()> {
        if self.last != Some(morton) {
            self.out.write_all(&self.row.to_le_bytes())?;
            self.codes.write_all(&morton.to_le_bytes())?;
            self.last = Some(morton);
        }
        self.row += 1;
        Ok(())
    }

    /// Flush both files.
    pub fn finish(mut self) -> io::Result<()> {
        self.out.flush()?;
        self.codes.flush()
    }
}

/// One row of a segment: what [`SegmentWriter::append`] takes, in row order.
///
/// **The code and its residual are carried, never a coordinate.** Both producers already hold the
/// pair and neither may re-derive it — a merge reads it off its inputs' mapped bytes, and
/// dequantise-then-requantise would move every point by up to a cell on every merge, silently
/// (write-path §7).
pub struct SegmentRow<'a> {
    pub tessera_id: TesseraId,
    pub morton: u32,
    /// The low half of the row's 64-bit interleaved position; `morton` is the high half.
    pub residual: u32,
    /// The declared scalars, positionally against the `scalar_schema` the writer was created with.
    pub scalars: &'a [ScalarValue],
    /// The row's value of the `k`th of the `indexed` columns the writer was created with, or
    /// [`ScalarValue::Null`]. Asked only of a row the bands hold ([`crate::bands::BandWriter`]).
    pub indexed: &'a dyn Fn(usize) -> io::Result<ScalarValue>,
}

/// A [`SegmentRow::indexed`] for a writer created with no indexed columns, which is never asked.
pub fn no_indexed(k: usize) -> io::Result<ScalarValue> {
    Err(io::Error::new(
        io::ErrorKind::InvalidInput,
        format!("no indexed column {k} was declared to this writer"),
    ))
}

/// Writes one segment's `morton.u32` and `columns.arrow` from rows arriving in
/// `(morton, tessera_id)` order, holding no row and no column.
///
/// See the module doc for why this is the only thing that knows the layout. Memory is the offset
/// table of any `Utf8` declared scalar (4 B/row — the one term that is not O(1) in rows) plus two
/// `BufWriter`s.
///
/// **That term is zero in every bundle a schema can currently produce**, and by refusal rather
/// than by accident: `render` is the only built placement and `render` on `utf8` is refused at
/// parse (per-point-attributes §4.3 — a per-row string is the vocabulary stored once per row). The
/// writer keeps the capability because the *format* admits it and a hand-written manifest may
/// declare one; what no schema can do is ask for it.
///
/// **The spools are native-endian; the artefacts are not.** A spool becomes an Arrow values buffer
/// in memory, where arrow reads it at the host's own endianness, so writing it little-endian would
/// corrupt every value on a big-endian host. `morton.u32` and `permutation.bin` are interchange
/// byte formats fixed little-endian by contracts §2.6 and stay so.
pub struct SegmentWriter {
    morton: BufWriter<File>,
    cuts: CutWriter,
    bands: crate::bands::BandWriter,
    dir: PathBuf,
    columns_path: PathBuf,
    schema: Arc<Schema>,
    /// One spool per schema column, in schema order: `tessera_id`, `residual`, then the declared
    /// scalars.
    columns: Vec<ColumnSpool>,
    rows: usize,
    /// The last `(morton, tessera_id)` appended — the row-order check's only state.
    last_key: Option<(u32, u64)>,
    spools: SpoolGuard,
}

impl SegmentWriter {
    /// Create the segment's files under `dir`, which must exist. `scalar_schema` is
    /// `columns.arrow`'s tail; `indexed` is the columns the bands copy beside it, whose values each
    /// row's [`SegmentRow::indexed`] answers.
    pub fn create(
        dir: &Path,
        scalar_schema: &[(String, ScalarType)],
        indexed: &[(String, ScalarType)],
    ) -> io::Result<Self> {
        let mut fields = fixed_fields();
        for (name, ty) in scalar_schema {
            fields.push(Field::new(name, arrow_type_of(*ty), false));
        }
        let schema = Arc::new(Schema::new(fields));

        // The guard is constructed **before** the first spool file, so a failure part-way through
        // this loop still unlinks the ones already created.
        let spools = SpoolGuard(
            (0..schema.fields().len())
                .map(|idx| dir.join(format!("columns.arrow.spool.{idx}")))
                .collect(),
        );
        let mut columns = Vec::with_capacity(schema.fields().len());
        for (idx, field) in schema.fields().iter().enumerate() {
            let kind = ColumnKind::of(field.data_type(), field.name())?;
            columns.push(ColumnSpool::create(&spools.0[idx], kind)?);
        }

        Ok(SegmentWriter {
            morton: BufWriter::new(File::create(dir.join("morton.u32"))?),
            cuts: CutWriter::create(dir)?,
            bands: crate::bands::BandWriter::create(dir, indexed)?,
            dir: dir.to_path_buf(),
            columns_path: dir.join("columns.arrow"),
            schema,
            columns,
            rows: 0,
            last_key: None,
            spools,
        })
    }

    /// Append the next row. Rows must arrive in `(morton, tessera_id)` order — contracts §2.6's
    /// row order, which `tile_ranges` binary-searches.
    ///
    /// **A `debug_assert`, matching what this function replaced, because the fail-closed backstop
    /// is at the read.** `MortonSlice::load` refuses a non-ascending `morton.u32` at every open,
    /// and the engine's merge unit loads the segment it just wrote before publishing it — so an
    /// out-of-order producer is a refused publication in release and a failed test in debug, never
    /// a bundle that serves nonsense.
    pub fn append(&mut self, row: SegmentRow<'_>) -> io::Result<()> {
        let key = (row.morton, row.tessera_id.raw());
        debug_assert!(
            self.last_key.is_none_or(|last| last <= key),
            "SegmentWriter::append: rows must arrive in (morton, tessera_id) order"
        );
        self.last_key = Some(key);

        self.morton.write_all(&row.morton.to_le_bytes())?;
        self.cuts.push(row.morton)?;
        self.bands
            .push(row.tessera_id.raw(), row.morton, row.residual, row.indexed)?;
        self.columns[0].append_u64(row.tessera_id.raw())?;
        self.columns[1].append_u32(row.residual)?;
        for (idx, spool) in self.columns.iter_mut().enumerate().skip(FIXED_COLUMN_COUNT) {
            let name = self.schema.field(idx).name();
            let value = row.scalars.get(idx - FIXED_COLUMN_COUNT).ok_or_else(|| {
                io::Error::new(
                    io::ErrorKind::InvalidInput,
                    format!(
                        "write_segment: tessera_id {} is missing scalar '{}' at index {}",
                        row.tessera_id.raw(),
                        name,
                        idx - FIXED_COLUMN_COUNT
                    ),
                )
            })?;
            spool.append(value, name, row.tessera_id)?;
        }
        self.rows += 1;
        Ok(())
    }

    /// Assemble `columns.arrow` from the spools, then the band file from it, and return the row
    /// count.
    ///
    /// The spools are mapped rather than read back, so the record batch's values buffers are the
    /// files themselves; the IPC writer copies them out once. They are unlinked on the way out of
    /// this function whether it succeeded or not — [`SpoolGuard`].
    pub fn finish(self) -> io::Result<usize> {
        let SegmentWriter {
            mut morton,
            cuts,
            bands,
            dir,
            columns_path,
            schema,
            columns,
            rows,
            spools,
            ..
        } = self;
        morton.flush()?;
        drop(morton);
        cuts.finish()?;

        let mut arrays: Vec<ArrayRef> = Vec::with_capacity(columns.len());
        for (spool, field) in columns.into_iter().zip(schema.fields()) {
            arrays.push(spool.into_array(rows, field.name())?);
        }
        let batch = RecordBatch::try_new(schema.clone(), arrays)
            .map_err(|e| io::Error::new(io::ErrorKind::InvalidData, e.to_string()))?;

        write_single_batch(&columns_path, &schema, &batch)?;
        // The mappings die with the batch, before `spools` unlinks the files they cover.
        drop(batch);
        drop(spools);
        bands.finish(&dir)?;
        Ok(rows)
    }
}

/// Unlinks the spool files it names, on every exit path from [`SegmentWriter`] — success, error
/// and panic alike.
///
/// **A destructor rather than a line at the bottom of `finish`**: the fold's spools are
/// corpus-sized (compaction §3, ~12 GB at 10⁹), so leaving them behind on a failure is a filled
/// device, which takes the whole write path down with it (write-path §1.3).
struct SpoolGuard(Vec<PathBuf>);

impl Drop for SpoolGuard {
    fn drop(&mut self) {
        for path in &self.0 {
            let _ = std::fs::remove_file(path);
        }
    }
}

/// The two columns contracts §2.6 fixes; everything after them is a declared scalar.
const FIXED_COLUMN_COUNT: usize = 2;

/// What kind of Arrow column a spool is accumulating — the types [`fixed_fields`] and
/// [`arrow_type_of`] between them can produce.
#[derive(Debug, Clone, Copy)]
pub(crate) enum ColumnKind {
    Bool,
    U8,
    U16,
    U32,
    U64,
    I8,
    I16,
    I32,
    I64,
    F32,
    F64,
    TimestampUs,
    Utf8,
}

impl ColumnKind {
    fn of(ty: &DataType, name: &str) -> io::Result<Self> {
        match ty {
            DataType::Boolean => Ok(ColumnKind::Bool),
            DataType::UInt8 => Ok(ColumnKind::U8),
            DataType::UInt16 => Ok(ColumnKind::U16),
            DataType::UInt32 => Ok(ColumnKind::U32),
            DataType::UInt64 => Ok(ColumnKind::U64),
            DataType::Int8 => Ok(ColumnKind::I8),
            DataType::Int16 => Ok(ColumnKind::I16),
            DataType::Int32 => Ok(ColumnKind::I32),
            DataType::Int64 => Ok(ColumnKind::I64),
            DataType::Float32 => Ok(ColumnKind::F32),
            DataType::Float64 => Ok(ColumnKind::F64),
            DataType::Timestamp(TimeUnit::Microsecond, None) => Ok(ColumnKind::TimestampUs),
            DataType::Utf8 => Ok(ColumnKind::Utf8),
            other => Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                format!("write_segment: column '{name}' has unsupported type {other:?}"),
            )),
        }
    }

    /// Whether this kind carries an offset table rather than fixed-width values.
    fn is_var_width(self) -> bool {
        matches!(self, ColumnKind::Utf8)
    }
}

/// One column's values, appended to a spool file as rows arrive.
pub(crate) struct ColumnSpool {
    kind: ColumnKind,
    writer: BufWriter<File>,
    /// [`ColumnKind::Bool`] only: the partial byte being packed, and how many of its bits are
    /// live. Flushed at eight, and padded at `into_array` — a trailing partial byte is real, and
    /// dropping it would lose up to seven rows' values with the row count still agreeing.
    bit_buf: u8,
    bit_len: u8,
    /// Var-width kinds only: Arrow's `i32` offset table, which has no file-backed form — a
    /// `LargeBinary`'s `i64` twin is what `PostingsSpool` holds for the same reason.
    offsets: Vec<i32>,
}

impl ColumnSpool {
    pub(crate) fn create(path: &Path, kind: ColumnKind) -> io::Result<Self> {
        // Read access as well as write: `into_array` maps the spool through this same handle, and
        // mapping a write-only descriptor fails with EACCES.
        let file = std::fs::OpenOptions::new()
            .read(true)
            .write(true)
            .create(true)
            .truncate(true)
            .open(path)?;
        Ok(ColumnSpool {
            kind,
            writer: BufWriter::new(file),
            bit_buf: 0,
            bit_len: 0,
            offsets: if kind.is_var_width() {
                vec![0]
            } else {
                Vec::new()
            },
        })
    }

    /// The `tessera_id` column, whose type [`fixed_fields`] fixes — no tag to check.
    fn append_u64(&mut self, value: u64) -> io::Result<()> {
        debug_assert!(matches!(self.kind, ColumnKind::U64));
        self.writer.write_all(&value.to_ne_bytes())
    }

    /// The `residual` column.
    pub(crate) fn append_u32(&mut self, value: u32) -> io::Result<()> {
        debug_assert!(matches!(self.kind, ColumnKind::U32));
        self.writer.write_all(&value.to_ne_bytes())
    }

    /// The running `i32` offset after appending `len` more bytes, refusing the overflow rather
    /// than wrapping — `PostingsSpool`'s `next_offset` rule, at `i32` because Arrow's `Utf8`
    /// offsets are 32-bit.
    fn next_offset(&self, len: usize, name: &str) -> io::Result<i32> {
        let last = *self
            .offsets
            .last()
            .expect("var-width spools hold a leading 0");
        i32::try_from(len)
            .ok()
            .and_then(|len| last.checked_add(len))
            .ok_or_else(|| {
                io::Error::new(
                    io::ErrorKind::InvalidInput,
                    format!("column '{name}' exceeds i32::MAX bytes"),
                )
            })
    }

    /// Append one declared scalar, refusing a tag that is not the column's declared type.
    ///
    /// **A mismatch fails the write rather than coercing or dropping.** Dropping shortens the
    /// column and shifts every later row of it into another row's place — every value present,
    /// every value against the wrong identity, and no error anywhere.
    fn append(&mut self, value: &ScalarValue, name: &str, tessera_id: TesseraId) -> io::Result<()> {
        match (self.kind, value) {
            // Packed into the spool a bit at a time, so the spool *is* the Arrow values buffer
            // and `into_array` can map it like every other column. Buffering a byte and flushing
            // it when full is the whole mechanism; `finish` pads the last partial byte.
            (ColumnKind::Bool, ScalarValue::Bool(v)) => {
                if *v {
                    self.bit_buf |= 1 << self.bit_len;
                }
                self.bit_len += 1;
                if self.bit_len == 8 {
                    self.writer.write_all(&[self.bit_buf])?;
                    self.bit_buf = 0;
                    self.bit_len = 0;
                }
                Ok(())
            }
            (ColumnKind::U8, ScalarValue::U8(v)) => self.writer.write_all(&v.to_ne_bytes()),
            (ColumnKind::U16, ScalarValue::U16(v)) => self.writer.write_all(&v.to_ne_bytes()),
            (ColumnKind::U32, ScalarValue::U32(v)) => self.writer.write_all(&v.to_ne_bytes()),
            (ColumnKind::U64, ScalarValue::U64(v)) => self.writer.write_all(&v.to_ne_bytes()),
            (ColumnKind::I8, ScalarValue::I8(v)) => self.writer.write_all(&v.to_ne_bytes()),
            (ColumnKind::I16, ScalarValue::I16(v)) => self.writer.write_all(&v.to_ne_bytes()),
            (ColumnKind::I32, ScalarValue::I32(v)) => self.writer.write_all(&v.to_ne_bytes()),
            (ColumnKind::I64, ScalarValue::I64(v)) => self.writer.write_all(&v.to_ne_bytes()),
            (ColumnKind::TimestampUs, ScalarValue::TimestampUs(v)) => {
                self.writer.write_all(&v.to_ne_bytes())
            }
            (ColumnKind::F32, ScalarValue::F32(v)) => self.writer.write_all(&v.to_ne_bytes()),
            (ColumnKind::F64, ScalarValue::F64(v)) => self.writer.write_all(&v.to_ne_bytes()),
            (ColumnKind::Utf8, ScalarValue::Utf8(v)) => {
                let next = self.next_offset(v.len(), name)?;
                self.writer.write_all(v.as_bytes())?;
                self.offsets.push(next);
                Ok(())
            }
            (kind, got) => Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                format!(
                    "write_segment: tessera_id {} scalar '{name}' expected {kind:?}, got {got:?}",
                    tessera_id.raw()
                ),
            )),
        }
    }

    /// Map the spool and build this column's array over it.
    pub(crate) fn into_array(self, rows: usize, name: &str) -> io::Result<ArrayRef> {
        let ColumnSpool {
            kind,
            mut writer,
            offsets,
            bit_buf,
            bit_len,
        } = self;
        // The trailing partial byte, if any. Without it a column whose row count is not a
        // multiple of eight loses up to seven values while `rows` still agrees — a short buffer
        // arrow would either refuse or read past, and neither says what happened.
        if bit_len > 0 {
            writer.write_all(&[bit_buf])?;
        }
        let file = writer
            .into_inner()
            .map_err(io::IntoInnerError::into_error)?;
        // The bytes are about to be read back through a memory map; they must be durable and
        // visible before the map is taken. Same rule as `PostingsSpool::finish`.
        file.sync_all()?;
        let len = file.metadata()?.len() as usize;

        // memmap2 rejects a zero-length map, and an empty typed array cannot be built from a
        // dangling `Buffer` either — `typed_column`'s alignment check would refuse it. Both
        // producers can legitimately write no rows, so this is the ordinary empty case.
        if len == 0 {
            return Ok(match kind {
                ColumnKind::Bool => Arc::new(BooleanArray::from(Vec::<bool>::new())) as ArrayRef,
                ColumnKind::U8 => Arc::new(UInt8Array::from(Vec::<u8>::new())),
                ColumnKind::U16 => Arc::new(UInt16Array::from(Vec::<u16>::new())),
                ColumnKind::U32 => Arc::new(UInt32Array::from(Vec::<u32>::new())),
                ColumnKind::U64 => Arc::new(UInt64Array::from(Vec::<u64>::new())),
                ColumnKind::I8 => Arc::new(Int8Array::from(Vec::<i8>::new())),
                ColumnKind::I16 => Arc::new(Int16Array::from(Vec::<i16>::new())),
                ColumnKind::I32 => Arc::new(Int32Array::from(Vec::<i32>::new())),
                ColumnKind::I64 => Arc::new(Int64Array::from(Vec::<i64>::new())),
                ColumnKind::TimestampUs => {
                    Arc::new(TimestampMicrosecondArray::from(Vec::<i64>::new()))
                }
                ColumnKind::F32 => Arc::new(Float32Array::from(Vec::<f32>::new())),
                ColumnKind::F64 => Arc::new(Float64Array::from(Vec::<f64>::new())),
                ColumnKind::Utf8 => Arc::new(
                    StringArray::try_new(
                        OffsetBuffer::new(ScalarBuffer::from(offsets)),
                        Buffer::from_vec(Vec::<u8>::new()),
                        None,
                    )
                    .map_err(|e| io::Error::new(io::ErrorKind::InvalidData, e.to_string()))?,
                ),
            });
        }

        // SAFETY: identical to `ColumnsRef::load`'s and `PostingsSpool::finish`'s mmap arm — `arc`
        // owns the mapping for as long as any `Buffer` built from it is alive (captured as the
        // buffer's `Allocation`), the mapping is valid for `len` bytes for its whole lifetime, and
        // memmap2 never returns a null base pointer.
        let mapping = unsafe { memmap2::Mmap::map(&file) }?;
        let arc: Arc<memmap2::Mmap> = Arc::new(mapping);
        let ptr = NonNull::new(arc.as_ptr() as *mut u8)
            .expect("memmap2::Mmap never returns a null base pointer");
        let buffer = unsafe { Buffer::from_custom_allocation(ptr, len, arc) };

        Ok(match kind {
            // The one column whose buffer is not `rows` elements wide: a bit each, so
            // `ceil(rows / 8)` bytes, which `BooleanBuffer` slices to `rows` itself.
            ColumnKind::Bool => {
                let needed = rows.div_ceil(8);
                if buffer.len() < needed {
                    return Err(io::Error::new(
                        io::ErrorKind::InvalidInput,
                        format!(
                            "column '{name}' has {} bytes, {rows} packed bits need {needed}",
                            buffer.len()
                        ),
                    ));
                }
                Arc::new(BooleanArray::new(
                    arrow::buffer::BooleanBuffer::new(buffer, 0, rows),
                    None,
                )) as ArrayRef
            }
            ColumnKind::U8 => Arc::new(UInt8Array::new(typed_column(name, buffer, rows)?, None)),
            ColumnKind::U16 => Arc::new(UInt16Array::new(typed_column(name, buffer, rows)?, None)),
            ColumnKind::U32 => Arc::new(UInt32Array::new(typed_column(name, buffer, rows)?, None)),
            ColumnKind::U64 => Arc::new(UInt64Array::new(typed_column(name, buffer, rows)?, None)),
            ColumnKind::I8 => Arc::new(Int8Array::new(typed_column(name, buffer, rows)?, None)),
            ColumnKind::I16 => Arc::new(Int16Array::new(typed_column(name, buffer, rows)?, None)),
            ColumnKind::I32 => Arc::new(Int32Array::new(typed_column(name, buffer, rows)?, None)),
            ColumnKind::I64 => Arc::new(Int64Array::new(typed_column(name, buffer, rows)?, None)),
            ColumnKind::TimestampUs => Arc::new(TimestampMicrosecondArray::new(
                typed_column(name, buffer, rows)?,
                None,
            )),
            ColumnKind::F32 => Arc::new(Float32Array::new(typed_column(name, buffer, rows)?, None)),
            ColumnKind::F64 => Arc::new(Float64Array::new(typed_column(name, buffer, rows)?, None)),
            ColumnKind::Utf8 => Arc::new(
                StringArray::try_new(OffsetBuffer::new(ScalarBuffer::from(offsets)), buffer, None)
                    .map_err(|e| io::Error::new(io::ErrorKind::InvalidData, e.to_string()))?,
            ),
        })
    }
}

/// The fixed, non-nullable fields of `columns.arrow` (contracts §2.6), in column order. The reader
/// (`read::validate_schema`) checks each per column.
///
/// `residual` is the *low* half of the point's 64-bit interleaved position; the high half is
/// the cell code in `morton.u32`, and concatenating them recovers the whole. It replaces the
/// `x`/`y` `f32` pair. With `priority` cut too (decision 0046), the fixed table is **two columns
/// and 12 bytes per row**, where the original four were 18. Nothing reads a coordinate off a
/// segment: what is stored is the position in the grid's own units, at 32 bits per axis rather
/// than an `f32`'s 24-bit mantissa.
fn fixed_fields() -> Vec<Field> {
    // No `priority` column (decision 0046). It was 2 B/row written and read by nothing at query
    // time — the selection comparator reads the full `tessera_id`, of which priority is the high
    // 16 bits — and format 1 is unpublished, so cutting it now is free where cutting it later is
    // a break. Re-adding it is additive (the reader matches columns by name) and is licensed the
    // day a measured prefix-scan optimisation asks for it.
    vec![
        Field::new("tessera_id", DataType::UInt64, false),
        Field::new("residual", DataType::UInt32, false),
    ]
}

/// The one writer path for `columns.arrow`: Arrow IPC file format, exactly one record batch
/// (the reader's `decode_single_batch` refuses anything else), uncompressed buffers, no fsync
/// (the build pipeline's manifest digests are what make a partially-written file detectable).
/// Every `columns.arrow` writer funnels through here, so equal batches produce equal bytes by
/// construction.
fn write_single_batch(path: &Path, schema: &Arc<Schema>, batch: &RecordBatch) -> io::Result<()> {
    let file = File::create(path)?;
    let mut writer = FileWriter::try_new(BufWriter::new(file), schema)
        .map_err(|e| io::Error::new(io::ErrorKind::InvalidData, e.to_string()))?;
    writer
        .write(batch)
        .map_err(|e| io::Error::new(io::ErrorKind::InvalidData, e.to_string()))?;
    writer
        .finish()
        .map_err(|e| io::Error::new(io::ErrorKind::InvalidData, e.to_string()))?;
    Ok(())
}

/// The Arrow type each declared scalar width becomes in `columns.arrow`'s schema. Paired with
/// `read::validate_schema`'s accepted set and [`ColumnKind::of`]: a type added to one and not the
/// others is a segment one writer emits and the reader refuses.
pub(crate) fn arrow_type_of(ty: ScalarType) -> DataType {
    match ty {
        ScalarType::Bool => DataType::Boolean,
        ScalarType::U8 => DataType::UInt8,
        ScalarType::U16 => DataType::UInt16,
        ScalarType::U32 => DataType::UInt32,
        ScalarType::U64 => DataType::UInt64,
        ScalarType::I8 => DataType::Int8,
        ScalarType::I16 => DataType::Int16,
        ScalarType::I32 => DataType::Int32,
        ScalarType::I64 => DataType::Int64,
        ScalarType::F32 => DataType::Float32,
        ScalarType::F64 => DataType::Float64,
        // `None` for the timezone: these are instants, and a per-column zone would be a second
        // place a time's meaning is decided.
        ScalarType::TimestampUs => DataType::Timestamp(TimeUnit::Microsecond, None),
        // **Unreachable, and `Utf8` rather than a panic.** `render` on either string type is
        // refused at the declaration, so no keyword column reaches the hot column's schema. If one
        // ever did, its rendered form would be the value's bytes — which is what
        // `ScalarValue::or_render_placeholder` substitutes for it — never the ordinal, which is a
        // per-layer index internal (records §4.3). The two must agree, so they are stated as one.
        ScalarType::Utf8 | ScalarType::Keyword | ScalarType::Text => DataType::Utf8,
    }
}

/// Check `buffer` can back `rows` values of `T` — long enough, and aligned to `T` (arrow's
/// `ScalarBuffer` conversion *panics* on misalignment; this turns both failure modes into
/// typed `InvalidInput` errors first). A buffer longer than `rows` values is fine — the tail
/// is sliced off — so a page-rounded mapping needs no trimming by the caller.
fn typed_column<T: ArrowNativeType>(
    name: &str,
    buffer: Buffer,
    rows: usize,
) -> io::Result<ScalarBuffer<T>> {
    let width = std::mem::size_of::<T>();
    let needed = rows.checked_mul(width).ok_or_else(|| {
        io::Error::new(
            io::ErrorKind::InvalidInput,
            format!("columns.arrow: {rows} rows of '{name}' overflow usize"),
        )
    })?;
    if buffer.len() < needed {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            format!(
                "columns.arrow: column '{name}' has {} bytes, {rows} rows of \
                 {width}-byte values need {needed}",
                buffer.len()
            ),
        ));
    }
    let align = std::mem::align_of::<T>();
    if buffer.as_ptr().align_offset(align) != 0 {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            format!(
                "columns.arrow: column '{name}' buffer is not {align}-byte aligned \
                 (arrow requires element alignment; page-aligned mmaps always satisfy this)"
            ),
        ));
    }
    Ok(ScalarBuffer::new(buffer, 0, rows))
}

/// Write `permutation.bin` (R4), the two-level paged entity→row map — see
/// [`crate::permutation`] for the byte layout and for why it is paged.
/// `items_in_row_order[i]` is the entity occupying row `i`; its slot gets `i`. Every other slot in
/// a page some entity does occupy reads the row-absent sentinel `0xFFFF_FFFF`, and a page no
/// entity occupies is not written at all.
///
/// Returns an error — never panics — if `bound` exceeds `2^32` (entity IDs are `u64` in general
/// but must fit `u32` in `bundle_format = 1`, R1, so no larger bound is satisfiable) or if any
/// entity ID is `>= bound`. The bound is checked before anything is allocated or opened, so an
/// unsatisfiable bound costs nothing; see [`PagePlan::new`].
pub fn write_permutation(
    path: &Path,
    items_in_row_order: &[EntityId],
    bound: u64,
) -> io::Result<()> {
    write_permutation_iter(path, items_in_row_order.iter().copied(), bound)
}

/// [`write_permutation`] over an iterator of entities in row order, so a caller holding its row
/// order in a packed form need not materialise a `Vec<EntityId>` (8 bytes per row) alongside it.
/// Byte-for-byte identical output — it is [`PermutationWriter`]'s second producer.
///
/// **The iterator is walked twice, which is why it must be `Clone`.** The first pass learns which
/// pages the view occupies; only then can the file be sized to them. The alternative — size for
/// every page, then compact — is what [`PermutationWriter::create`] does for the scatter producer
/// that cannot know its pages up front, and it costs a `bound × 4`-byte fill that this path
/// avoids entirely. Every caller here holds its row order in a `Vec` or derives it from a range,
/// so the second walk is a re-read of memory already in hand and nothing is buffered to enable it.
pub fn write_permutation_iter<I>(path: &Path, items_in_row_order: I, bound: u64) -> io::Result<()>
where
    I: IntoIterator<Item = EntityId>,
    I::IntoIter: Clone,
{
    let items = items_in_row_order.into_iter();
    let plan = PagePlan::of_entities(bound, items.clone())?;
    let mut writer = PermutationWriter::create_planned(path, &plan)?;
    for (row, entity_id) in items.enumerate() {
        let row = u32::try_from(row).map_err(|_| {
            io::Error::new(
                io::ErrorKind::InvalidInput,
                format!("write_permutation: row index {row} does not fit in u32"),
            )
        })?;
        writer.set(entity_id, row)?;
    }
    writer.finish()
}

/// Which pages of entity space a view occupies — everything [`PermutationWriter::create_planned`]
/// needs to lay the file out before a single row is scattered into it.
///
/// One `bool` per page: 64 KB at the `u32` entity ceiling, and a few bytes for the views that
/// motivate the paging.
#[derive(Debug, Clone)]
pub struct PagePlan {
    bound: u64,
    present: Vec<bool>,
}

impl PagePlan {
    /// An empty plan over `[0, bound)`.
    ///
    /// **The bound ceiling is checked here, before any file exists.** A caller deriving a bound
    /// from a corrupt `entity_id_high_water` gets a refusal from a `Vec` allocation, not from a
    /// filled volume.
    pub fn new(bound: u64) -> io::Result<Self> {
        check_bound(bound)?;
        let pages = usize::try_from(pages_for(bound)).map_err(|_| {
            io::Error::new(
                io::ErrorKind::InvalidInput,
                format!("write_permutation: bound {bound} does not fit in usize"),
            )
        })?;
        Ok(PagePlan {
            bound,
            present: vec![false; pages],
        })
    }

    /// Declare that `entity` will be given a row.
    pub fn insert(&mut self, entity: EntityId) -> io::Result<()> {
        let raw = entity.raw();
        if raw >= self.bound {
            return Err(out_of_bound(raw, self.bound));
        }
        self.present[(raw >> PAGE_SHIFT) as usize] = true;
        Ok(())
    }

    /// The plan for a view whose entities are `entities`, in any order.
    pub fn of_entities<I: IntoIterator<Item = EntityId>>(
        bound: u64,
        entities: I,
    ) -> io::Result<Self> {
        let mut plan = PagePlan::new(bound)?;
        for entity in entities {
            plan.insert(entity)?;
        }
        Ok(plan)
    }

    /// How many pages carry slots.
    pub fn present_pages(&self) -> usize {
        self.present.iter().filter(|&&p| p).count()
    }
}

/// Writes `permutation.bin` **through a mapping**, scattering `perm[entity] = row` in any order.
///
/// **Scatter order is free, which is why this is a writer and not an iterator.** The compaction
/// fold's pass 1 emits rows in `(morton, tessera_id)` order and learns `perm[entity]` in that
/// order, which is *not* entity order — a sequential writer would have to buffer the whole
/// mapping to reorder, which is the cost this avoids. Bytes live in a mapping rather than in a
/// `Vec` for the same reason: anonymous memory the kernel can only swap, against page cache it can
/// write back, and `Permutation::load` treats the file the same way at read.
///
/// # Two constructors, one output
///
/// [`Self::create_planned`] is for a producer that knows its pages up front — the build, which
/// holds its row order in memory — and writes only those pages. [`Self::create`] is for the fold,
/// which does not: it lays out **every** page of `[0, bound)`, scatters into them, and compacts
/// the present ones down at [`Self::finish`]. The compaction moves each present page to its
/// canonical slot, which is never above where it already sits, so it is one forward `copy_within`
/// pass and a truncation.
///
/// The two produce **the same bytes for the same mapping** — the encoding is canonical, so there
/// is nothing left for them to disagree about, and
/// `segment_roundtrip::a_scattered_permutation_is_byte_identical_to_a_sequential_one` pins it.
/// What differs is what the write costs: the scatter path still writes `page_count × 256 KiB` of
/// sentinel before the first row lands (4 GB at a 10⁹-entity bound), and the artifact is small
/// only after the truncation. **The paging shrinks what a sparse view stores and maps, not what
/// the fold's scatter transiently dirties** — that is the same figure compaction §3's pre-flight
/// budget already carries.
pub struct PermutationWriter {
    file: File,
    map: memmap2::MmapMut,
    bound: u64,
    payload_start: usize,
    /// Where each page's slots live in the payload, or [`PAGE_ABSENT`] — one entry per page of
    /// `[0, bound)`, so this is also the page count. In the scatter layout it is the identity
    /// until [`Self::finish`] compacts it.
    slot_of_page: Vec<u32>,
    /// Scatter layout only: which pages a `set` has landed in. `None` is the planned layout, whose
    /// present set is fixed at construction.
    touched: Option<Vec<bool>>,
}

impl PermutationWriter {
    /// Create `path` holding every page of `[0, bound)`, every slot the row-absent sentinel,
    /// compacted to the pages actually written at [`Self::finish`].
    ///
    /// **The fill is not optional and not free.** A freshly extended file reads as zeros, and zero
    /// is row 0 — a real row belonging to a real entity — so an unfilled slot would serve one
    /// entity's coordinates under every id that never got a row. The sentinel is `0xFFFF_FFFF`, so
    /// this writes the whole payload as `0xFF` up front.
    ///
    /// **The bound ceiling is checked before the file is opened, and that ordering is the point.**
    /// The fill above is proportional to `bound`, so validating it afterwards means writing
    /// `bound × 4` bytes to disk in order to discover the caller asked for something no entity
    /// could ever occupy — 32 GB for a bound of `2^33`, paid in full before the error is raised.
    pub fn create(path: &Path, bound: u64) -> io::Result<Self> {
        let plan = PagePlan::new(bound)?;
        let page_count = plan.present.len();
        let mut writer = Self::open(path, bound, page_count, page_count)?;
        // The identity: page `p` scatters into slot `p`, and `finish` moves it down to the slot
        // its rank among the touched pages gives it.
        for (page, slot) in writer.slot_of_page.iter_mut().enumerate() {
            *slot = page as u32;
        }
        writer.touched = Some(vec![false; page_count]);
        Ok(writer)
    }

    /// Create `path` holding exactly the pages `plan` declares, every slot the row-absent
    /// sentinel. A [`Self::set`] for an entity in an undeclared page is an error, not a silent
    /// drop.
    pub fn create_planned(path: &Path, plan: &PagePlan) -> io::Result<Self> {
        let mut writer = Self::open(path, plan.bound, plan.present.len(), plan.present_pages())?;
        let mut next: u32 = 0;
        for (page, present) in plan.present.iter().enumerate() {
            if *present {
                writer.slot_of_page[page] = next;
                next += 1;
            }
        }
        Ok(writer)
    }

    fn open(path: &Path, bound: u64, page_count: usize, payload_pages: usize) -> io::Result<Self> {
        let payload_start = payload_start(page_count);
        let len = payload_start + payload_pages * PAGE_BYTES;
        let file = std::fs::OpenOptions::new()
            .read(true)
            .write(true)
            .create(true)
            .truncate(true)
            .open(path)?;
        file.set_len(len as u64)?;

        // SAFETY: this process created and sized the file two statements ago and holds the only
        // handle to it; nothing else maps or truncates it for the writer's lifetime. The
        // concurrently-truncated-backing-file hazard `Permutation::load` documents is the same
        // one and is an operational property, not one this call can check.
        let mut map = unsafe { memmap2::MmapMut::map_mut(&file) }?;
        map[..4].copy_from_slice(PERMUTATION_MAGIC);
        map[4..6].copy_from_slice(&PERMUTATION_VERSION.to_le_bytes());
        map[6..8].copy_from_slice(&(PAGE_SHIFT as u16).to_le_bytes());
        map[8..16].copy_from_slice(&bound.to_le_bytes());
        map[16..20].copy_from_slice(&(page_count as u32).to_le_bytes());
        // `present_count` and the directory are written at `finish`, when the scatter layout knows
        // them. The header's remaining bytes and the padding stay zero, which is what the reader
        // requires of the padding.
        map[payload_start..].fill(0xFF);

        Ok(PermutationWriter {
            file,
            map,
            bound,
            payload_start,
            slot_of_page: vec![PAGE_ABSENT; page_count],
            touched: None,
        })
    }

    /// Record that `entity` occupies `row`. Every check [`write_permutation_iter`] made is made
    /// here, at the same cost — the duplicate test is a slot read the scatter was doing anyway.
    ///
    /// **R1's "entity ids fit `u32`" is enforced by the bound, not by a second test here.**
    /// The constructors refuse any bound above `2^32`, so `raw < self.bound` already implies
    /// `raw < 2^32` and a separate u32-fit check could never fire. Reinstating one would read as
    /// live defence against a case the constructor has already made unreachable.
    pub fn set(&mut self, entity: EntityId, row: u32) -> io::Result<()> {
        let raw = entity.raw();
        if raw >= self.bound {
            return Err(out_of_bound(raw, self.bound));
        }
        // Closes a format ambiguity permanently: a row index equal to the row-absent sentinel
        // would be indistinguishable on disk from "entity has no row".
        if row == PERMUTATION_ABSENT {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                format!(
                    "write_permutation: row index {row} collides with the row-absent sentinel \
                     (0xFFFF_FFFF)"
                ),
            ));
        }
        let page = (raw >> PAGE_SHIFT) as usize;
        let slot = self.slot_of_page[page];
        if slot == PAGE_ABSENT {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                format!(
                    "write_permutation: entity id {raw} falls in page {page}, which the page plan \
                     does not hold"
                ),
            ));
        }
        if let Some(touched) = self.touched.as_mut() {
            touched[page] = true;
        }
        let at = self.payload_start
            + slot as usize * PAGE_BYTES
            + ((raw as usize) & (PAGE_ENTRIES - 1)) * 4;
        let existing = u32::from_le_bytes(
            self.map[at..at + 4]
                .try_into()
                .expect("a four-byte window is four bytes"),
        );
        if existing != PERMUTATION_ABSENT {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                format!(
                    "write_permutation: entity id {raw} appears at both row {existing} and row \
                     {row}"
                ),
            ));
        }
        // Explicit little-endian, never a native cast: the slot array is an interchange byte
        // format (contracts §2.6), unlike the spools above, which become in-memory Arrow buffers.
        self.map[at..at + 4].copy_from_slice(&row.to_le_bytes());
        Ok(())
    }

    /// Compact the scatter layout, write the directory, and flush.
    pub fn finish(mut self) -> io::Result<()> {
        if let Some(touched) = self.touched.take() {
            // Each present page moves to the slot its rank gives it, which is at or below where it
            // sits — so a forward pass never overwrites a page it has yet to move.
            let mut next: u32 = 0;
            for (page, written) in touched.iter().enumerate() {
                if !written {
                    self.slot_of_page[page] = PAGE_ABSENT;
                    continue;
                }
                let from = self.payload_start + page * PAGE_BYTES;
                let to = self.payload_start + next as usize * PAGE_BYTES;
                if from != to {
                    self.map.copy_within(from..from + PAGE_BYTES, to);
                }
                self.slot_of_page[page] = next;
                next += 1;
            }
        }
        let present: u32 = self
            .slot_of_page
            .iter()
            .filter(|&&slot| slot != PAGE_ABSENT)
            .count() as u32;
        self.map[20..24].copy_from_slice(&present.to_le_bytes());
        for (page, &slot) in self.slot_of_page.iter().enumerate() {
            let at = PERMUTATION_HEADER_BYTES + page * 4;
            self.map[at..at + 4].copy_from_slice(&slot.to_le_bytes());
        }
        let len = self.payload_start + present as usize * PAGE_BYTES;

        let PermutationWriter { map, file, .. } = self;
        map.flush()?;
        // The mapping is dropped before the file shrinks: reading through a mapping past a
        // truncation is a fault, not an error, and the scatter layout always shrinks unless every
        // page was written.
        drop(map);
        file.set_len(len as u64)?;
        Ok(())
    }
}

fn check_bound(bound: u64) -> io::Result<()> {
    if bound > PERMUTATION_MAX_BOUND {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            format!(
                "write_permutation: bound {bound} exceeds the largest satisfiable bound \
                 {PERMUTATION_MAX_BOUND} (entity ids must fit u32 in bundle_format = 1)"
            ),
        ));
    }
    Ok(())
}

fn out_of_bound(raw: u64, bound: u64) -> io::Error {
    io::Error::new(
        io::ErrorKind::InvalidInput,
        format!("write_permutation: entity id {raw} is out of bound (bound = {bound})"),
    )
}
