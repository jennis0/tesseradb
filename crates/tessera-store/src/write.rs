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
//! **What knows the layout is [`fixed_fields`] and [`write_single_batch`], and there is a third
//! caller of them.** [`write_columns_from_parts`] is the batch build's own two-column path, which
//! hands over file-backed `Buffer`s it has already assembled and has no row-at-a-time shape to
//! offer. It is not a second writer in the sense the rule forbids — it emits the same schema
//! through the same IPC invocation, which is what
//! `segment_roundtrip::write_columns_from_parts_matches_write_columns_byte_for_byte` pins — but it
//! is a second *assembler*, and a change to the column set has to reach both.

use std::fs::File;
use std::io::{self, BufWriter, Write};
use std::path::{Path, PathBuf};
use std::ptr::NonNull;
use std::sync::Arc;

use arrow::array::{ArrayRef, BinaryArray, Float32Array, StringArray, UInt32Array, UInt64Array};
use arrow::buffer::{Buffer, OffsetBuffer, ScalarBuffer};
use arrow::datatypes::{ArrowNativeType, DataType, Field, Schema};
use arrow::ipc::writer::FileWriter;
use arrow::record_batch::RecordBatch;

use tessera_spatial::split32;
use tessera_spatial::tiler::{ScalarType, ScalarValue, TilerItem};
use tessera_types::{EntityId, TesseraId};

const PERMUTATION_MAGIC: &[u8; 4] = b"TSPM";
const PERMUTATION_VERSION: u16 = 1;
const PERMUTATION_RESERVED: u16 = 0;
const PERMUTATION_ABSENT: u32 = 0xFFFF_FFFF;

/// Write `columns.arrow` (Arrow IPC file format, one record batch, uncompressed buffers) and
/// `morton.u32` (raw little-endian `u32` codes, no header) into `dir`.
///
/// `items` and `codes` must already be in row order (i.e. the output of
/// [`tessera_spatial::tiler::sort_batch`]) and the same length; row *i*'s Morton code is
/// `codes[i]`. `scalar_schema` declares the name and Arrow type of each item's trailing
/// `scalars`, in the order they appear in `TilerItem::scalars`.
///
/// **One of [`SegmentWriter`]'s producers, not a second writer** — see the module doc. The caller
/// already holds every row, so nothing here is streamed *in*; what the delegation buys is that
/// this path and the merge's cannot drift apart in layout.
pub fn write_segment(
    dir: &Path,
    items: &[TilerItem],
    codes: &[u32],
    scalar_schema: &[(String, ScalarType)],
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

    let mut writer = SegmentWriter::create(dir, scalar_schema)?;
    for (item, code) in items.iter().zip(codes) {
        writer.append(SegmentRow {
            tessera_id: item.tessera_id,
            morton: *code,
            // The residual is the low half of the same `split32` whose high half the tiler
            // returned as this row's code, so the stored pair is one splitting of one position
            // rather than two derivations that could disagree.
            residual: split32(item.qx, item.qy).1,
            scalars: &item.scalars,
        })?;
    }
    writer.finish()?;
    Ok(())
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
}

/// Writes one segment's `morton.u32` and `columns.arrow` from rows arriving in
/// `(morton, tessera_id)` order, holding no row and no column.
///
/// See the module doc for why this is the only thing that knows the layout. Memory is the offset
/// table of any `Utf8` declared scalar (4 B/row — the one term that is not O(1) in rows, and zero
/// in every bundle that exists, since `tessera-build` writes `declared_scalars` empty
/// unconditionally, contracts §2.2) plus two `BufWriter`s.
///
/// **The spools are native-endian; the artefacts are not.** A spool becomes an Arrow values buffer
/// in memory, where arrow reads it at the host's own endianness, so writing it little-endian would
/// corrupt every value on a big-endian host. `morton.u32` and `permutation.bin` are interchange
/// byte formats fixed little-endian by contracts §2.6 and stay so.
pub struct SegmentWriter {
    morton: BufWriter<File>,
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
    /// Create the segment's files under `dir`, which must exist.
    pub fn create(dir: &Path, scalar_schema: &[(String, ScalarType)]) -> io::Result<Self> {
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

    /// Assemble `columns.arrow` from the spools and return the row count.
    ///
    /// The spools are mapped rather than read back, so the record batch's values buffers are the
    /// files themselves; the IPC writer copies them out once. They are unlinked on the way out of
    /// this function whether it succeeded or not — [`SpoolGuard`].
    pub fn finish(self) -> io::Result<usize> {
        let SegmentWriter {
            mut morton,
            columns_path,
            schema,
            columns,
            rows,
            spools,
            ..
        } = self;
        morton.flush()?;
        drop(morton);

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
        Ok(rows)
    }
}

/// The fields of `external-ids.arrow` (contracts §2.4), in column order. One definition, so
/// [`RunWriter`] and [`crate::flush::write_external_id_run`] cannot drift apart.
fn external_id_fields() -> Vec<Field> {
    vec![
        Field::new("external_id", DataType::Binary, false),
        Field::new("entity_id", DataType::UInt32, false),
    ]
}

/// Writes one `external-ids.arrow` from `(external_id, entity)` pairs arriving in **ascending key
/// order**, holding no pair.
///
/// [`SegmentWriter`]'s counterpart on the entity-space side, and for the same reason: the
/// entity-space coalesce and compaction's pass 3 (compaction §3) both merge runs that are already
/// key-sorted, and neither may accumulate every key to do it. The flush path — which holds its
/// rows anyway — is the second producer, via [`crate::flush::write_external_id_run`].
///
/// Memory is the `i32` offset table (4 B/key) and two `BufWriter`s.
pub(crate) struct RunWriter {
    path: PathBuf,
    keys: ColumnSpool,
    entities: ColumnSpool,
    rows: usize,
    last_key: Option<Vec<u8>>,
    spools: SpoolGuard,
}

impl RunWriter {
    /// Create the run at `path` (the `external-ids.arrow` file itself), spooling beside it.
    pub(crate) fn create(path: &Path) -> io::Result<Self> {
        let spool = |i: usize| {
            let mut name = path.as_os_str().to_os_string();
            name.push(format!(".spool.{i}"));
            PathBuf::from(name)
        };
        let spools = SpoolGuard(vec![spool(0), spool(1)]);
        Ok(RunWriter {
            path: path.to_path_buf(),
            keys: ColumnSpool::create(&spools.0[0], ColumnKind::Binary)?,
            entities: ColumnSpool::create(&spools.0[1], ColumnKind::U32)?,
            rows: 0,
            last_key: None,
            spools,
        })
    }

    /// Append one pair. Keys must arrive **strictly ascending** — the sidecar binary-searches this
    /// file and verifies the ordering at open, so an out-of-order producer is a refusal there
    /// rather than a wrong answer here. Checked as a `debug_assert`, matching [`SegmentWriter`].
    pub(crate) fn append(&mut self, key: &[u8], entity: u32) -> io::Result<()> {
        debug_assert!(
            self.last_key.as_deref().is_none_or(|last| last < key),
            "RunWriter::append: keys must arrive strictly ascending"
        );
        self.last_key = Some(key.to_vec());
        self.keys.append_bytes(key, "external_id")?;
        self.entities.append_u32(entity)?;
        self.rows += 1;
        Ok(())
    }

    /// Assemble `external-ids.arrow` and return the pair count.
    pub(crate) fn finish(self) -> io::Result<usize> {
        let RunWriter {
            path,
            keys,
            entities,
            rows,
            spools,
            ..
        } = self;
        let schema = Arc::new(Schema::new(external_id_fields()));
        let columns: Vec<ArrayRef> = vec![
            keys.into_array(rows, "external_id")?,
            entities.into_array(rows, "entity_id")?,
        ];
        let batch = RecordBatch::try_new(schema.clone(), columns)
            .map_err(|e| io::Error::new(io::ErrorKind::InvalidData, e.to_string()))?;
        write_single_batch(&path, &schema, &batch)?;
        drop(batch);
        drop(spools);
        Ok(rows)
    }
}

/// Unlinks the spool files it names, on every exit path from [`SegmentWriter`] or [`RunWriter`] —
/// success, error and panic alike.
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

/// What kind of Arrow column a spool is accumulating — the four types [`fixed_fields`] and
/// [`arrow_type_of`] between them can produce.
#[derive(Debug, Clone, Copy)]
pub(crate) enum ColumnKind {
    U64,
    U32,
    F32,
    Utf8,
    /// `external-ids.arrow`'s key column (contracts §2.4). Same shape as [`ColumnKind::Utf8`] —
    /// `i32` offsets over a values buffer — but external ids are arbitrary bytes, not UTF-8.
    Binary,
}

impl ColumnKind {
    fn of(ty: &DataType, name: &str) -> io::Result<Self> {
        match ty {
            DataType::UInt64 => Ok(ColumnKind::U64),
            DataType::UInt32 => Ok(ColumnKind::U32),
            DataType::Float32 => Ok(ColumnKind::F32),
            DataType::Utf8 => Ok(ColumnKind::Utf8),
            DataType::Binary => Ok(ColumnKind::Binary),
            other => Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                format!("write_segment: column '{name}' has unsupported type {other:?}"),
            )),
        }
    }

    /// Whether this kind carries an offset table rather than fixed-width values.
    fn is_var_width(self) -> bool {
        matches!(self, ColumnKind::Utf8 | ColumnKind::Binary)
    }
}

/// One column's values, appended to a spool file as rows arrive.
///
/// `pub(crate)` because [`crate::coalesce`]'s external-id run writer spools its two columns the
/// same way — one spool implementation, for [`SegmentWriter`]'s reason.
pub(crate) struct ColumnSpool {
    kind: ColumnKind,
    writer: BufWriter<File>,
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

    /// The `residual` column, and `external-ids.arrow`'s `entity_id` column.
    pub(crate) fn append_u32(&mut self, value: u32) -> io::Result<()> {
        debug_assert!(matches!(self.kind, ColumnKind::U32));
        self.writer.write_all(&value.to_ne_bytes())
    }

    /// One `Binary` value — an external id (contracts §1: arbitrary bytes, ≤ 64).
    pub(crate) fn append_bytes(&mut self, value: &[u8], name: &str) -> io::Result<()> {
        debug_assert!(matches!(self.kind, ColumnKind::Binary));
        let next = self.next_offset(value.len(), name)?;
        self.writer.write_all(value)?;
        self.offsets.push(next);
        Ok(())
    }

    /// The running `i32` offset after appending `len` more bytes, refusing the overflow rather
    /// than wrapping — `PostingsSpool`'s `next_offset` rule, at `i32` because Arrow's `Binary`
    /// and `Utf8` offsets are 32-bit.
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
            (ColumnKind::U64, ScalarValue::U64(v)) => self.writer.write_all(&v.to_ne_bytes()),
            (ColumnKind::F32, ScalarValue::F32(v)) => self.writer.write_all(&v.to_ne_bytes()),
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
            writer,
            offsets,
        } = self;
        let file = writer.into_inner().map_err(io::IntoInnerError::into_error)?;
        // The bytes are about to be read back through a memory map; they must be durable and
        // visible before the map is taken. Same rule as `PostingsSpool::finish`.
        file.sync_all()?;
        let len = file.metadata()?.len() as usize;

        // memmap2 rejects a zero-length map, and an empty typed array cannot be built from a
        // dangling `Buffer` either — `typed_column`'s alignment check would refuse it. Both
        // producers can legitimately write no rows, so this is the ordinary empty case.
        if len == 0 {
            return Ok(match kind {
                ColumnKind::U64 => Arc::new(UInt64Array::from(Vec::<u64>::new())) as ArrayRef,
                ColumnKind::U32 => Arc::new(UInt32Array::from(Vec::<u32>::new())),
                ColumnKind::F32 => Arc::new(Float32Array::from(Vec::<f32>::new())),
                ColumnKind::Utf8 => Arc::new(
                    StringArray::try_new(
                        OffsetBuffer::new(ScalarBuffer::from(offsets)),
                        Buffer::from_vec(Vec::<u8>::new()),
                        None,
                    )
                    .map_err(|e| io::Error::new(io::ErrorKind::InvalidData, e.to_string()))?,
                ),
                ColumnKind::Binary => Arc::new(
                    BinaryArray::try_new(
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
            ColumnKind::U64 => Arc::new(UInt64Array::new(
                typed_column(name, buffer, rows)?,
                None,
            )) as ArrayRef,
            ColumnKind::U32 => Arc::new(UInt32Array::new(typed_column(name, buffer, rows)?, None)),
            ColumnKind::F32 => Arc::new(Float32Array::new(typed_column(name, buffer, rows)?, None)),
            ColumnKind::Utf8 => Arc::new(
                StringArray::try_new(OffsetBuffer::new(ScalarBuffer::from(offsets)), buffer, None)
                    .map_err(|e| io::Error::new(io::ErrorKind::InvalidData, e.to_string()))?,
            ),
            ColumnKind::Binary => Arc::new(
                BinaryArray::try_new(OffsetBuffer::new(ScalarBuffer::from(offsets)), buffer, None)
                    .map_err(|e| io::Error::new(io::ErrorKind::InvalidData, e.to_string()))?,
            ),
        })
    }
}

/// The fixed, non-nullable fields of `columns.arrow` (contracts §2.6), in column order. One
/// definition, so the writers below cannot drift apart in name, type or nullability — the reader
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

fn arrow_type_of(ty: ScalarType) -> DataType {
    match ty {
        ScalarType::U64 => DataType::UInt64,
        ScalarType::F32 => DataType::Float32,
        ScalarType::Utf8 => DataType::Utf8,
    }
}

/// Write `morton.u32` (contracts §2.6: raw little-endian `u32` codes, no header) from an
/// iterator of codes in row order.
///
/// Streaming, so the codes need never exist as one slice: the batch build holds its row order in
/// a packed record array and would otherwise have to materialise a second copy alongside the
/// segment columns, which are already the largest thing it allocates.
pub fn write_morton_codes<I: IntoIterator<Item = u32>>(path: &Path, codes: I) -> io::Result<()> {
    let mut writer = BufWriter::new(File::create(path)?);
    for code in codes {
        writer.write_all(&code.to_le_bytes())?;
    }
    writer.flush()
}

/// Write `columns.arrow` from columns that are already in row order — the fixed two columns of
/// contracts §2.6, no declared scalars.
///
/// Takes each column **by value** so the `Vec`s become the Arrow buffers with no copy. This
/// record batch is the largest single structure the batch build materialises (at 10^9 rows,
/// 8+4 bytes per row), so a copy here would be another twelve gigabytes. Produces
/// byte-for-byte what [`write_segment`] writes for the same rows and no scalars. Since the
/// 2026-07-31 rework this function is a thin wrapper over [`write_columns_from_parts`]; there
/// is one code path.
pub fn write_columns(
    path: &Path,
    tessera_id: Vec<u64>,
    residual: Vec<u32>,
) -> io::Result<()> {
    let rows = tessera_id.len();
    if residual.len() != rows {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            format!(
                "write_columns: column 'residual' has {} rows, tessera_id has {rows}",
                residual.len()
            ),
        ));
    }

    // Delegation, not duplication: `Buffer::from_vec` takes ownership of each `Vec`'s
    // allocation with no copy, and `write_columns_from_parts` builds the identical record
    // batch (same schema, same zero-null primitive arrays over the same bytes) through the
    // same `write_single_batch` path — so the delegated output is byte-for-byte what this
    // function wrote before it delegated.
    write_columns_from_parts(
        path,
        Buffer::from_vec(tessera_id),
        Buffer::from_vec(residual),
        rows,
    )
}

/// [`write_columns`], but from raw column bytes instead of `Vec`s: `tessera_id` as `rows`
/// little-endian `u64`s and `residual` as `rows` little-endian `u32`s, taken **without
/// copying** — each `Buffer` *becomes* the record batch's values buffer. This is the batch
/// build's handover point for file-backed columns: at 3×10⁹ rows the two `Vec`s of
/// [`write_columns`] are 36 GB of anonymous memory, whereas mmap-backed `Buffer`s
/// (`Buffer::from_custom_allocation` over a scratch file) cost address space only.
///
/// **Alignment**: Arrow requires each values buffer to be aligned to its element type —
/// 8 bytes for `tessera_id`, 4 for `residual` (`ScalarBuffer` refuses less). An mmap is
/// page-aligned, so a buffer covering a mapping from offset 0 always qualifies; only a caller
/// slicing a buffer at an offset that is not a multiple of the element size can violate it,
/// and that (like a buffer shorter than `rows` elements) is rejected here as an
/// `InvalidInput` error — fail closed, never a panic from inside arrow.
///
/// Output is byte-identical to [`write_columns`] over the same values: same schema
/// ([`fixed_fields`]), same null-free primitive arrays (no validity buffers — every column is
/// contractually non-nullable, R4), same single-batch writer ([`write_single_batch`]).
pub fn write_columns_from_parts(
    path: &Path,
    tessera_id: Buffer,
    residual: Buffer,
    rows: usize,
) -> io::Result<()> {
    let tessera_id: ScalarBuffer<u64> = typed_column("tessera_id", tessera_id, rows)?;
    let residual: ScalarBuffer<u32> = typed_column("residual", residual, rows)?;

    let tessera_id = UInt64Array::new(tessera_id, None);

    let schema = Arc::new(Schema::new(fixed_fields()));
    let columns: Vec<ArrayRef> = vec![
        Arc::new(tessera_id),
        Arc::new(UInt32Array::new(residual, None)),
    ];
    let batch = RecordBatch::try_new(schema.clone(), columns)
        .map_err(|e| io::Error::new(io::ErrorKind::InvalidData, e.to_string()))?;

    write_single_batch(path, &schema, &batch)
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
            format!("write_columns_from_parts: {rows} rows of '{name}' overflow usize"),
        )
    })?;
    if buffer.len() < needed {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            format!(
                "write_columns_from_parts: column '{name}' has {} bytes, {rows} rows of \
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
                "write_columns_from_parts: column '{name}' buffer is not {align}-byte aligned \
                 (arrow requires element alignment; page-aligned mmaps always satisfy this)"
            ),
        ));
    }
    Ok(ScalarBuffer::new(buffer, 0, rows))
}

/// Write `permutation.bin` (R4): `"TSPM"` ‖ u16 version=1 ‖ u16 reserved=0 ‖ u64 `bound` ‖
/// `bound` little-endian `u32` slots, one per entity ID in `[0, bound)`. `items_in_row_order[i]`
/// is the entity occupying row `i`; its slot gets `i`. Every other slot (entity IDs never
/// assigned a row in this segment) reads the row-absent sentinel `0xFFFF_FFFF`.
///
/// Returns an error — never panics — if any entity ID is `>= bound` or `>= 2^32` (entity IDs
/// are `u64` in general but must fit `u32` in `bundle_format = 1`, R1).
pub fn write_permutation(
    path: &Path,
    items_in_row_order: &[EntityId],
    bound: u64,
) -> io::Result<()> {
    write_permutation_iter(path, items_in_row_order.iter().copied(), bound)
}

/// [`write_permutation`] over an iterator of entities in row order, so a caller holding its row
/// order in a packed form need not materialise a `Vec<EntityId>` (8 bytes per row) alongside the
/// slot array. Byte-for-byte identical output — it is [`PermutationWriter`]'s second producer.
pub fn write_permutation_iter<I: IntoIterator<Item = EntityId>>(
    path: &Path,
    items_in_row_order: I,
    bound: u64,
) -> io::Result<()> {
    let mut writer = PermutationWriter::create(path, bound)?;
    for (row, entity_id) in items_in_row_order.into_iter().enumerate() {
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

/// `permutation.bin`'s header: `"TSPM"` ‖ u16 version=1 ‖ u16 reserved=0 ‖ u64 `bound` (R4).
const PERMUTATION_HEADER_BYTES: usize = 16;

/// Writes `permutation.bin` **through a mapping**, scattering `perm[entity] = row` in any order.
///
/// The slot array is `bound` `u32`s — 4 GB at 10⁹ entities — and that is inherent to the format,
/// which R4 defines as a dense entity-indexed array. What is *not* inherent is where those bytes
/// live. Built as a `Vec` they are anonymous memory the kernel can only swap; written through a
/// mapping they are page cache, which can be written back under pressure and which
/// `Permutation::load` already treats the same way at read. Compaction's pass 1 scatters this over
/// the whole entity space while four other corpus-scale things are in flight (compaction §3), so
/// the difference is the fold's pre-flight budget passing or failing.
///
/// **Scatter order is free, which is why this is a writer and not an iterator.** Pass 1 emits rows
/// in `(morton, tessera_id)` order and learns `perm[entity]` in that order, which is *not* entity
/// order — a sequential writer would have to buffer the whole array to reorder, which is the cost
/// this avoids.
pub struct PermutationWriter {
    map: memmap2::MmapMut,
    bound: u64,
}

impl PermutationWriter {
    /// Create `path` sized for `bound` entities, every slot the row-absent sentinel.
    ///
    /// **The fill is not optional and not free.** A freshly extended file reads as zeros, and zero
    /// is row 0 — a real row belonging to a real entity — so an unfilled slot would serve one
    /// entity's coordinates under every id that never got a row. The sentinel is `0xFFFF_FFFF`, so
    /// this writes `bound × 4` bytes of `0xFF` up front.
    pub fn create(path: &Path, bound: u64) -> io::Result<Self> {
        let slots_bytes = usize::try_from(bound)
            .ok()
            .and_then(|b| b.checked_mul(4))
            .ok_or_else(|| {
                io::Error::new(
                    io::ErrorKind::InvalidInput,
                    format!("write_permutation: bound {bound} does not fit in usize"),
                )
            })?;
        let file = std::fs::OpenOptions::new()
            .read(true)
            .write(true)
            .create(true)
            .truncate(true)
            .open(path)?;
        file.set_len((PERMUTATION_HEADER_BYTES + slots_bytes) as u64)?;

        // SAFETY: this process created and sized the file two statements ago and holds the only
        // handle to it; nothing else maps or truncates it for the writer's lifetime. The
        // concurrently-truncated-backing-file hazard `Permutation::load` documents is the same
        // one and is an operational property, not one this call can check.
        let mut map = unsafe { memmap2::MmapMut::map_mut(&file) }?;
        map[..4].copy_from_slice(PERMUTATION_MAGIC);
        map[4..6].copy_from_slice(&PERMUTATION_VERSION.to_le_bytes());
        map[6..8].copy_from_slice(&PERMUTATION_RESERVED.to_le_bytes());
        map[8..16].copy_from_slice(&bound.to_le_bytes());
        map[PERMUTATION_HEADER_BYTES..].fill(0xFF);

        Ok(PermutationWriter { map, bound })
    }

    /// Record that `entity` occupies `row`. Every check [`write_permutation_iter`] made is made
    /// here, at the same cost — the duplicate test is a slot read the scatter was doing anyway.
    pub fn set(&mut self, entity: EntityId, row: u32) -> io::Result<()> {
        let raw = entity.raw();
        if raw >= self.bound {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                format!(
                    "write_permutation: entity id {raw} is out of bound (bound = {})",
                    self.bound
                ),
            ));
        }
        if raw >= (1u64 << 32) {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                format!(
                    "write_permutation: entity id {raw} does not fit in u32 (bundle_format = 1)"
                ),
            ));
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
        let at = PERMUTATION_HEADER_BYTES + (raw as usize) * 4;
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

    pub fn finish(self) -> io::Result<()> {
        self.map.flush()
    }
}
