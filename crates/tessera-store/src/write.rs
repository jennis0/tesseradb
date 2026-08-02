//! Segment file writers: `columns.arrow`, `morton.u32`, `permutation.bin` (contracts §2.1/§2.6).
//! Byte formats here are fixed by the contracts spec — do not vary them.

use std::fs::File;
use std::io::{self, BufWriter, Write};
use std::path::Path;
use std::sync::Arc;

use arrow::array::{ArrayRef, Float32Array, StringArray, UInt16Array, UInt32Array, UInt64Array};
use arrow::buffer::{Buffer, ScalarBuffer};
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

    debug_assert!(
        codes.windows(2).all(|w| w[0] <= w[1]),
        "write_segment: codes must be non-decreasing (caller must pass sort_batch's output)"
    );

    write_columns_arrow(&dir.join("columns.arrow"), items, scalar_schema)?;
    write_morton_u32(&dir.join("morton.u32"), codes)?;
    Ok(())
}

/// The three fixed, non-nullable fields of `columns.arrow` (contracts §2.6 r6), in column
/// order. One definition, so the three writers below cannot drift apart in name, type or
/// nullability — the reader (`read::validate_schema`) checks all three per column.
///
/// `residual` is the *low* half of the point's 64-bit interleaved position; the high half is
/// the cell code in `morton.u32`, and concatenating them recovers the whole. It replaces the
/// `x`/`y` `f32` pair, which is why this table is three columns and 14 bytes per row rather
/// than four and 18. Nothing reads a coordinate off a segment: what is stored is the position
/// in the grid's own units, at 32 bits per axis rather than an `f32`'s 24-bit mantissa.
fn fixed_fields() -> Vec<Field> {
    vec![
        Field::new("tessera_id", DataType::UInt64, false),
        Field::new("residual", DataType::UInt32, false),
        Field::new("priority", DataType::UInt16, false),
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

fn write_columns_arrow(
    path: &Path,
    items: &[TilerItem],
    scalar_schema: &[(String, ScalarType)],
) -> io::Result<()> {
    let mut fields = fixed_fields();
    for (name, ty) in scalar_schema {
        fields.push(Field::new(name, arrow_type_of(*ty), false));
    }
    let schema = Arc::new(Schema::new(fields));

    let tessera_id: ArrayRef = Arc::new(UInt64Array::from_iter_values(
        items.iter().map(|i| i.tessera_id.raw()),
    ));
    // The residual is the low half of `split32` — the same call whose high half the tiler
    // returned as this row's Morton code, so the stored pair is one splitting of one position
    // rather than two derivations that could disagree.
    let residual: ArrayRef = Arc::new(UInt32Array::from_iter_values(
        items.iter().map(|i| split32(i.qx, i.qy).1),
    ));
    // `priority` is derived here, from the `tessera_id` the item already carries — the one
    // place this column is computed (contracts §2.6 r6, `TesseraId::priority()`); neither the
    // tiler nor `write_columns` below recomputes the shift inline.
    let priority: ArrayRef = Arc::new(UInt16Array::from_iter_values(
        items.iter().map(|i| i.tessera_id.priority()),
    ));

    let mut columns: Vec<ArrayRef> = vec![tessera_id, residual, priority];
    for (idx, (name, ty)) in scalar_schema.iter().enumerate() {
        columns.push(build_scalar_column(items, idx, *ty, name)?);
    }

    let batch = RecordBatch::try_new(schema.clone(), columns)
        .map_err(|e| io::Error::new(io::ErrorKind::InvalidData, e.to_string()))?;

    write_single_batch(path, &schema, &batch)
}

fn arrow_type_of(ty: ScalarType) -> DataType {
    match ty {
        ScalarType::U64 => DataType::UInt64,
        ScalarType::F32 => DataType::Float32,
        ScalarType::Utf8 => DataType::Utf8,
    }
}

/// Build one declared-scalar column at position `idx` of every item's `scalars`, checking each
/// value's tag matches the declared `ty` (a mismatch is a caller bug — fail closed rather than
/// silently coercing or dropping the row).
fn build_scalar_column(
    items: &[TilerItem],
    idx: usize,
    ty: ScalarType,
    name: &str,
) -> io::Result<ArrayRef> {
    fn value_at<'a>(item: &'a TilerItem, idx: usize, name: &str) -> io::Result<&'a ScalarValue> {
        item.scalars.get(idx).ok_or_else(|| {
            io::Error::new(
                io::ErrorKind::InvalidInput,
                format!(
                    "write_segment: tessera_id {} is missing scalar '{}' at index {}",
                    item.tessera_id.raw(),
                    name,
                    idx
                ),
            )
        })
    }

    match ty {
        ScalarType::U64 => {
            let mut values = Vec::with_capacity(items.len());
            for item in items {
                match value_at(item, idx, name)? {
                    ScalarValue::U64(v) => values.push(*v),
                    other => return Err(scalar_type_mismatch(item, name, "U64", other)),
                }
            }
            Ok(Arc::new(UInt64Array::from(values)))
        }
        ScalarType::F32 => {
            let mut values = Vec::with_capacity(items.len());
            for item in items {
                match value_at(item, idx, name)? {
                    ScalarValue::F32(v) => values.push(*v),
                    other => return Err(scalar_type_mismatch(item, name, "F32", other)),
                }
            }
            Ok(Arc::new(Float32Array::from(values)))
        }
        ScalarType::Utf8 => {
            let mut values = Vec::with_capacity(items.len());
            for item in items {
                match value_at(item, idx, name)? {
                    ScalarValue::Utf8(v) => values.push(v.as_str()),
                    other => return Err(scalar_type_mismatch(item, name, "Utf8", other)),
                }
            }
            Ok(Arc::new(StringArray::from(values)))
        }
    }
}

fn scalar_type_mismatch(
    item: &TilerItem,
    name: &str,
    expected: &str,
    got: &ScalarValue,
) -> io::Error {
    io::Error::new(
        io::ErrorKind::InvalidInput,
        format!(
            "write_segment: tessera_id {} scalar '{}' expected {}, got {:?}",
            item.tessera_id.raw(),
            name,
            expected,
            got
        ),
    )
}

fn write_morton_u32(path: &Path, codes: &[u32]) -> io::Result<()> {
    write_morton_codes(path, codes.iter().copied())
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

/// Write `columns.arrow` from columns that are already in row order — the fixed three columns of
/// contracts §2.6, no declared scalars.
///
/// Takes each column **by value** so the `Vec`s become the Arrow buffers with no copy. This
/// record batch is the largest single structure the batch build materialises (at 10^9 rows,
/// 8+4+2 bytes per row), so a copy here would be another fourteen gigabytes. Produces
/// byte-for-byte what [`write_segment`] writes for the same rows and no scalars.
///
/// `priority` is derived from `tessera_id` via `TesseraId::priority` — the same one place
/// `write_columns_arrow` derives it — so the two build paths are byte-identical by
/// construction rather than by agreement (contracts §2.6 r6, 2026-07-30 fold). Since the
/// 2026-07-31 rework this function is a thin wrapper over [`write_columns_from_parts`], which
/// does that derivation and the writing; there is one code path.
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
    // batch (same schema, same zero-null primitive arrays over the same bytes, same priority
    // derivation) through the same `write_single_batch` path — so the delegated output is
    // byte-for-byte what this function wrote before it delegated.
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
/// `priority` is still derived here, row by row, from the `tessera_id` buffer via
/// [`TesseraId::priority`] — the same single definition site every `columns.arrow` writer uses
/// (contracts §2.6 r6, 2026-07-30 fold). Its transient `Vec<u16>` (2 bytes × `rows`) is the
/// only allocation proportional to the input and is bounded, accepted cost.
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
    let priority: Vec<u16> = tessera_id
        .values()
        .iter()
        .map(|&id| TesseraId::new(id).priority())
        .collect();

    let schema = Arc::new(Schema::new(fixed_fields()));
    let columns: Vec<ArrayRef> = vec![
        Arc::new(tessera_id),
        Arc::new(UInt32Array::new(residual, None)),
        Arc::new(UInt16Array::from(priority)),
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
/// slot array it is about to build. Byte-for-byte identical output.
///
/// The slot array itself is still `bound` `u32`s wide — 4 GB at 10^9 entities. That is inherent
/// to the format (R4 defines `permutation.bin` as a dense entity-indexed array) and is the
/// ledgered, accepted cost; it is one contiguous allocation, not a per-item one.
pub fn write_permutation_iter<I: IntoIterator<Item = EntityId>>(
    path: &Path,
    items_in_row_order: I,
    bound: u64,
) -> io::Result<()> {
    let bound_usize = usize::try_from(bound).map_err(|_| {
        io::Error::new(
            io::ErrorKind::InvalidInput,
            format!("write_permutation: bound {bound} does not fit in usize"),
        )
    })?;

    let mut slots = vec![PERMUTATION_ABSENT; bound_usize];
    for (row, entity_id) in items_in_row_order.into_iter().enumerate() {
        let raw = entity_id.raw();
        if raw >= bound {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                format!("write_permutation: entity id {raw} is out of bound (bound = {bound})"),
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
        let row_u32 = u32::try_from(row).map_err(|_| {
            io::Error::new(
                io::ErrorKind::InvalidInput,
                format!("write_permutation: row index {row} does not fit in u32"),
            )
        })?;
        // Closes a format ambiguity permanently: a row index equal to the row-absent
        // sentinel would be indistinguishable on disk from "entity has no row".
        if row_u32 == PERMUTATION_ABSENT {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                format!(
                    "write_permutation: row index {row} collides with the row-absent sentinel \
                     (0xFFFF_FFFF)"
                ),
            ));
        }
        let slot = raw as usize;
        if slots[slot] != PERMUTATION_ABSENT {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                format!(
                    "write_permutation: entity id {} appears at both row {} and row {}",
                    raw, slots[slot], row
                ),
            ));
        }
        slots[slot] = row_u32;
    }

    let mut writer = BufWriter::new(File::create(path)?);
    writer.write_all(PERMUTATION_MAGIC)?;
    writer.write_all(&PERMUTATION_VERSION.to_le_bytes())?;
    writer.write_all(&PERMUTATION_RESERVED.to_le_bytes())?;
    writer.write_all(&bound.to_le_bytes())?;
    for slot in &slots {
        writer.write_all(&slot.to_le_bytes())?;
    }
    writer.flush()
}
