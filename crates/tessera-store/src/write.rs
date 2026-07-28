//! Segment file writers: `columns.arrow`, `morton.u64`, `permutation.bin` (contracts §2.1/§2.6,
//! Reference Sheet R4). Byte formats here are fixed by the contracts spec — do not vary them.

use std::fs::File;
use std::io::{self, BufWriter, Write};
use std::path::Path;
use std::sync::Arc;

use arrow::array::{ArrayRef, Float32Array, StringArray, UInt16Array, UInt32Array, UInt64Array};
use arrow::datatypes::{DataType, Field, Schema};
use arrow::ipc::writer::FileWriter;
use arrow::record_batch::RecordBatch;

use tessera_spatial::tiler::{ScalarType, ScalarValue, TilerItem};
use tessera_types::EntityId;

const PERMUTATION_MAGIC: &[u8; 4] = b"TSPM";
const PERMUTATION_VERSION: u16 = 1;
const PERMUTATION_RESERVED: u16 = 0;
const PERMUTATION_ABSENT: u32 = 0xFFFF_FFFF;

/// Write `columns.arrow` (Arrow IPC file format, one record batch, uncompressed buffers) and
/// `morton.u64` (raw little-endian `u64` codes, no header) into `dir`.
///
/// `items` and `codes` must already be in row order (i.e. the output of
/// [`tessera_spatial::tiler::sort_batch`]) and the same length; row *i*'s Morton code is
/// `codes[i]`. `scalar_schema` declares the name and Arrow type of each item's trailing
/// `scalars`, in the order they appear in `TilerItem::scalars`.
pub fn write_segment(
    dir: &Path,
    items: &[TilerItem],
    codes: &[u64],
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
    write_morton_u64(&dir.join("morton.u64"), codes)?;
    Ok(())
}

fn write_columns_arrow(
    path: &Path,
    items: &[TilerItem],
    scalar_schema: &[(String, ScalarType)],
) -> io::Result<()> {
    let mut fields = vec![
        Field::new("entity_id", DataType::UInt64, false),
        Field::new("x", DataType::Float32, false),
        Field::new("y", DataType::Float32, false),
        Field::new("node_id", DataType::UInt32, false),
        Field::new("priority", DataType::UInt16, false),
    ];
    for (name, ty) in scalar_schema {
        fields.push(Field::new(name, arrow_type_of(*ty), false));
    }
    let schema = Arc::new(Schema::new(fields));

    let entity_id: ArrayRef = Arc::new(UInt64Array::from_iter_values(
        items.iter().map(|i| i.entity_id.raw()),
    ));
    let x: ArrayRef = Arc::new(Float32Array::from_iter_values(items.iter().map(|i| i.x)));
    let y: ArrayRef = Arc::new(Float32Array::from_iter_values(items.iter().map(|i| i.y)));
    let node_id: ArrayRef = Arc::new(UInt32Array::from_iter_values(
        items.iter().map(|i| i.node_id),
    ));
    let priority: ArrayRef = Arc::new(UInt16Array::from_iter_values(
        items.iter().map(|i| i.priority),
    ));

    let mut columns: Vec<ArrayRef> = vec![entity_id, x, y, node_id, priority];
    for (idx, (name, ty)) in scalar_schema.iter().enumerate() {
        columns.push(build_scalar_column(items, idx, *ty, name)?);
    }

    let batch = RecordBatch::try_new(schema.clone(), columns)
        .map_err(|e| io::Error::new(io::ErrorKind::InvalidData, e.to_string()))?;

    let file = File::create(path)?;
    let mut writer = FileWriter::try_new(BufWriter::new(file), &schema)
        .map_err(|e| io::Error::new(io::ErrorKind::InvalidData, e.to_string()))?;
    writer
        .write(&batch)
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
                    "write_segment: entity {} is missing scalar '{}' at index {}",
                    item.entity_id.raw(),
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
            "write_segment: entity {} scalar '{}' expected {}, got {:?}",
            item.entity_id.raw(),
            name,
            expected,
            got
        ),
    )
}

fn write_morton_u64(path: &Path, codes: &[u64]) -> io::Result<()> {
    let mut writer = BufWriter::new(File::create(path)?);
    for code in codes {
        writer.write_all(&code.to_le_bytes())?;
    }
    writer.flush()
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
    for entity_id in items_in_row_order {
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
    }

    let bound_usize = usize::try_from(bound).map_err(|_| {
        io::Error::new(
            io::ErrorKind::InvalidInput,
            format!("write_permutation: bound {bound} does not fit in usize"),
        )
    })?;

    let mut slots = vec![PERMUTATION_ABSENT; bound_usize];
    for (row, entity_id) in items_in_row_order.iter().enumerate() {
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
        let slot = entity_id.raw() as usize;
        if slots[slot] != PERMUTATION_ABSENT {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                format!(
                    "write_permutation: entity id {} appears at both row {} and row {}",
                    entity_id.raw(),
                    slots[slot],
                    row
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
