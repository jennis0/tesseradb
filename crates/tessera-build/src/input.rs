//! Parquet input readers for the batch build.
//!
//! Two inputs, both read as record-batch streams so the reader's own memory stays bounded even
//! when the file does not (the *build's* in-memory item vector is the memory ceiling, not the
//! decoder's):
//!
//! * **points** — `entity_id` plus geometry. Geometry is accepted either as explicit `x`/`y`
//!   columns or, when the corpus stores Morton codes instead (the Phase 0
//!   `data/scaled/geometry.parquet` does), as a `morton` column that is de-interleaved back to
//!   grid-cell coordinates. See [`read_points`].
//! * **pairs** — the exploded `(entity_id, term_id)` relation.
//!
//! Both honour a `limit`: `entity_id < limit` selects a prefix of entity space, which is a
//! whole coherent corpus because entity IDs are append-only (I9, dataset §4.1). Row groups
//! whose statistics prove they hold no qualifying row are skipped outright — at the Phase 0
//! scales the pairs relation is billions of rows and the prefix is a few hundred thousand.

use std::collections::HashMap;
use std::fs::File;
use std::path::Path;

use arrow::array::{Array, Float32Array, Float64Array, UInt32Array, UInt64Array};
use arrow::datatypes::DataType;
use arrow::record_batch::RecordBatch;
use parquet::arrow::arrow_reader::ParquetRecordBatchReaderBuilder;
use parquet::file::metadata::ParquetMetaData;
use parquet::file::statistics::Statistics;
use tessera_spatial::Extent;

use crate::error::{BuildError, Result};

/// One input point: its source-corpus entity ID (which becomes the external ID) and geometry.
#[derive(Debug, Clone, Copy)]
pub struct PointRow {
    pub source_id: u64,
    pub x: f32,
    pub y: f32,
}

/// The only extent under which the Morton input branch is meaningful: the grid's own
/// coordinates, `[0, 65536)` on both axes (contracts §2.5 — the grid is 2^16 x 2^16).
pub const IDENTITY_EXTENT: Extent = Extent {
    x_min: 0.0,
    x_max: 65536.0,
    y_min: 0.0,
    y_max: 65536.0,
};

/// Read `points`, keeping rows with `source_id < limit` when `limit` is `Some`.
///
/// Accepted schemas (checked in this order):
/// 1. `entity_id` + `x` + `y` — coordinates used as given, quantised against `extent`.
/// 2. `entity_id` + `morton` — the 32-bit Morton code is de-interleaved into its `(x_cell,
///    y_cell)` grid coordinates and those are returned as the coordinates. This reproduces the
///    source corpus's own Morton codes **exactly** — but only against [`IDENTITY_EXTENT`], where
///    `cell(v) = floor(v / 65536 × 65536) = v` for an integer `v ≤ 65535`. Under any other
///    extent the cell indices would be re-quantised as if they were coordinates in that extent's
///    units, silently collapsing or stretching the grid while `MANIFEST.json` went on declaring
///    the extent the caller passed — a bundle whose geometry and whose declared quantisation
///    disagree. So this branch **requires** the identity extent and errors otherwise; a corpus
///    with real coordinates must ship `x`/`y` and take branch 1.
pub fn read_points(path: &Path, extent: &Extent, limit: Option<u64>) -> Result<Vec<PointRow>> {
    let mut out = Vec::new();
    scan_points(path, extent, limit, |row| out.push(row))?;
    Ok(out)
}

/// The streaming form of [`read_points`]: calls `visit` once per selected row and never holds
/// more than one decoded record batch. The batch build uses this so the points file — 10⁹ rows
/// in the Phase 0 corpus — can be traversed several times without ever being materialised.
pub fn scan_points<F: FnMut(PointRow)>(
    path: &Path,
    extent: &Extent,
    limit: Option<u64>,
    mut visit: F,
) -> Result<()> {
    let file = File::open(path).map_err(|e| BuildError::io(path, e))?;
    let builder =
        ParquetRecordBatchReaderBuilder::try_new(file).map_err(|e| BuildError::parquet(path, e))?;
    let schema = builder.schema().clone();

    // Statistics live against the *file's* column order; batches come back in the projected
    // order. Keep the two index spaces apart deliberately — conflating them would silently read
    // the wrong column.
    let id_idx_in_file = column_index(path, &schema, "entity_id")?;
    let wanted: Vec<&str> =
        if schema.column_with_name("x").is_some() && schema.column_with_name("y").is_some() {
            vec!["entity_id", "x", "y"]
        } else if schema.column_with_name("morton").is_some() {
            if *extent != IDENTITY_EXTENT {
                return Err(BuildError::Schema {
                    path: path.to_path_buf(),
                    detail: format!(
                        "this points file stores Morton codes rather than coordinates, which is \
                         exact only against the grid's own extent (0,65536,0,65536); \
                         ({},{},{},{}) was given. Pass the identity extent, or supply a points \
                         file with 'x' and 'y' columns.",
                        extent.x_min, extent.x_max, extent.y_min, extent.y_max
                    ),
                });
            }
            vec!["entity_id", "morton"]
        } else {
            return Err(BuildError::Schema {
                path: path.to_path_buf(),
                detail: "points file needs either 'x' and 'y' columns or a 'morton' column".into(),
            });
        };

    // Project: the Phase 0 corpus carries columns this build has no use for, and at 10^9 rows
    // not decoding them is the difference between one pass and two.
    let mut roots = Vec::with_capacity(wanted.len());
    for name in &wanted {
        roots.push(column_index(path, &schema, name)?);
    }
    let projection = parquet::arrow::ProjectionMask::roots(builder.parquet_schema(), roots);

    let keep = prunable_row_groups(builder.metadata(), id_idx_in_file, limit);
    let reader = builder
        .with_row_groups(keep)
        .with_projection(projection)
        .with_batch_size(65_536)
        .build()
        .map_err(|e| BuildError::parquet(path, e))?;

    let projected = arrow::array::RecordBatchReader::schema(&reader);
    let id_idx = column_index(path, &projected, "entity_id")?;
    let geometry = if wanted.contains(&"x") {
        Geometry::Xy(
            column_index(path, &projected, "x")?,
            column_index(path, &projected, "y")?,
        )
    } else {
        Geometry::Morton(column_index(path, &projected, "morton")?)
    };

    for batch in reader {
        let batch = batch.map_err(|e| BuildError::arrow(path, e))?;
        let ids = read_u64_column(path, &batch, id_idx, "entity_id")?;
        match geometry {
            Geometry::Xy(xi, yi) => {
                let xs = read_f32_column(path, &batch, xi, "x")?;
                let ys = read_f32_column(path, &batch, yi, "y")?;
                for i in 0..batch.num_rows() {
                    if limit.is_some_and(|l| ids[i] >= l) {
                        continue;
                    }
                    visit(PointRow {
                        source_id: ids[i],
                        x: xs[i],
                        y: ys[i],
                    });
                }
            }
            Geometry::Morton(mi) => {
                let codes = read_u64_column(path, &batch, mi, "morton")?;
                for i in 0..batch.num_rows() {
                    if limit.is_some_and(|l| ids[i] >= l) {
                        continue;
                    }
                    let code = u32::try_from(codes[i]).map_err(|_| BuildError::Schema {
                        path: path.to_path_buf(),
                        detail: format!("morton code {} does not fit in u32", codes[i]),
                    })?;
                    let (cx, cy) = deinterleave(code);
                    visit(PointRow {
                        source_id: ids[i],
                        x: cx as f32,
                        y: cy as f32,
                    });
                }
            }
        }
    }
    Ok(())
}

/// Read `pairs` (`entity_id`, `term_id`), keeping rows with `entity_id < limit`, grouped into
/// each source entity's term list. Lists are returned sorted and deduplicated: the label set is
/// a *set*, and downstream (the signature key, the postings writer) depends on it being one.
pub fn read_pairs(path: &Path, limit: Option<u64>) -> Result<HashMap<u64, Vec<u64>>> {
    let mut grouped: HashMap<u64, Vec<u64>> = HashMap::new();
    scan_pairs(path, limit, |source_id, term| {
        grouped.entry(source_id).or_default().push(term)
    })?;
    for terms in grouped.values_mut() {
        terms.sort_unstable();
        terms.dedup();
    }
    Ok(grouped)
}

/// The streaming form of [`read_pairs`]: calls `visit(source_entity_id, source_term_id)` once
/// per selected row, in file order, holding only one decoded record batch. Rows are **not**
/// grouped, sorted or deduplicated — that is the caller's business, and at 1.72 × 10⁹ pairs it
/// is the difference between a bounded traversal and a 70 GB `HashMap`.
pub fn scan_pairs<F: FnMut(u64, u64)>(path: &Path, limit: Option<u64>, mut visit: F) -> Result<()> {
    let file = File::open(path).map_err(|e| BuildError::io(path, e))?;
    let builder =
        ParquetRecordBatchReaderBuilder::try_new(file).map_err(|e| BuildError::parquet(path, e))?;
    let schema = builder.schema().clone();
    let id_idx = column_index(path, &schema, "entity_id")?;
    let term_idx = column_index(path, &schema, "term_id")?;

    let keep = prunable_row_groups(builder.metadata(), id_idx, limit);
    let reader = builder
        .with_row_groups(keep)
        .with_batch_size(65_536)
        .build()
        .map_err(|e| BuildError::parquet(path, e))?;

    for batch in reader {
        let batch = batch.map_err(|e| BuildError::arrow(path, e))?;
        let ids = read_u64_column(path, &batch, id_idx, "entity_id")?;
        let terms = read_u64_column(path, &batch, term_idx, "term_id")?;
        for i in 0..batch.num_rows() {
            if limit.is_some_and(|l| ids[i] >= l) {
                continue;
            }
            visit(ids[i], terms[i]);
        }
    }
    Ok(())
}

#[derive(Debug, Clone, Copy)]
enum Geometry {
    Xy(usize, usize),
    Morton(usize),
}

/// Inverse of the Morton interleave (contracts §2.5 / R2): bit `2i` of `code` is bit `i` of the
/// x cell, bit `2i+1` is bit `i` of the y cell.
pub fn deinterleave(code: u32) -> (u16, u16) {
    (compact(code), compact(code >> 1))
}

/// Gather the even bits of `v` into the low 16 bits (the inverse of `spread`).
fn compact(v: u32) -> u16 {
    let mut x = v & 0x5555_5555;
    x = (x | (x >> 1)) & 0x3333_3333;
    x = (x | (x >> 2)) & 0x0F0F_0F0F;
    x = (x | (x >> 4)) & 0x00FF_00FF;
    x = (x | (x >> 8)) & 0x0000_FFFF;
    x as u16
}

/// Row groups worth reading: those whose `entity_id` statistics do not prove every row is
/// `>= limit`. Returns *all* row groups when there is no limit or no usable statistic — the
/// filter is an optimisation and must never drop a row it cannot prove is excluded.
fn prunable_row_groups(meta: &ParquetMetaData, id_idx: usize, limit: Option<u64>) -> Vec<usize> {
    let all = || (0..meta.num_row_groups()).collect::<Vec<_>>();
    let Some(limit) = limit else { return all() };

    let mut keep = Vec::new();
    for rg in 0..meta.num_row_groups() {
        let column = meta.row_group(rg).column(id_idx);
        let min = column.statistics().and_then(statistic_min);
        match min {
            Some(min) if min >= limit => continue,
            _ => keep.push(rg),
        }
    }
    keep
}

fn statistic_min(stats: &Statistics) -> Option<u64> {
    match stats {
        Statistics::Int32(s) => s.min_opt().map(|v| *v as u64),
        Statistics::Int64(s) => s.min_opt().and_then(|v| u64::try_from(*v).ok()),
        _ => None,
    }
}

fn column_index(path: &Path, schema: &arrow::datatypes::Schema, name: &str) -> Result<usize> {
    schema
        .column_with_name(name)
        .map(|(i, _)| i)
        .ok_or_else(|| BuildError::Schema {
            path: path.to_path_buf(),
            detail: format!("missing required column '{name}'"),
        })
}

/// Read an integer column as `u64`, accepting the widths a Parquet writer may have chosen.
fn read_u64_column(path: &Path, batch: &RecordBatch, idx: usize, name: &str) -> Result<Vec<u64>> {
    let column = batch.column(idx);
    if column.null_count() > 0 {
        return Err(BuildError::Schema {
            path: path.to_path_buf(),
            detail: format!("column '{name}' contains nulls"),
        });
    }
    let values: Vec<u64> = match column.data_type() {
        DataType::UInt64 => column
            .as_any()
            .downcast_ref::<UInt64Array>()
            .expect("checked data type")
            .values()
            .to_vec(),
        DataType::UInt32 => column
            .as_any()
            .downcast_ref::<UInt32Array>()
            .expect("checked data type")
            .values()
            .iter()
            .map(|v| *v as u64)
            .collect(),
        DataType::Int64 | DataType::Int32 => {
            let cast = arrow::compute::cast(column, &DataType::UInt64)
                .map_err(|e| BuildError::arrow(path, e))?;
            cast.as_any()
                .downcast_ref::<UInt64Array>()
                .expect("cast to UInt64")
                .values()
                .to_vec()
        }
        other => {
            return Err(BuildError::Schema {
                path: path.to_path_buf(),
                detail: format!("column '{name}' has unsupported type {other:?}"),
            })
        }
    };
    Ok(values)
}

fn read_f32_column(path: &Path, batch: &RecordBatch, idx: usize, name: &str) -> Result<Vec<f32>> {
    let column = batch.column(idx);
    if column.null_count() > 0 {
        return Err(BuildError::Schema {
            path: path.to_path_buf(),
            detail: format!("column '{name}' contains nulls"),
        });
    }
    match column.data_type() {
        DataType::Float32 => Ok(column
            .as_any()
            .downcast_ref::<Float32Array>()
            .expect("checked data type")
            .values()
            .to_vec()),
        DataType::Float64 => Ok(column
            .as_any()
            .downcast_ref::<Float64Array>()
            .expect("checked data type")
            .values()
            .iter()
            .map(|v| *v as f32)
            .collect()),
        other => Err(BuildError::Schema {
            path: path.to_path_buf(),
            detail: format!("column '{name}' has unsupported type {other:?}"),
        }),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn deinterleave_inverts_the_worked_example() {
        // R2's worked example: x_cell = 6, y_cell = 3 -> code 30.
        assert_eq!(deinterleave(30), (6, 3));
    }

    #[test]
    fn deinterleave_inverts_interleave_across_the_grid() {
        for x in [0u16, 1, 2, 255, 4096, 65534, 65535] {
            for y in [0u16, 1, 7, 300, 65535] {
                let code = tessera_spatial::interleave(x, y).raw();
                assert_eq!(deinterleave(code), (x, y));
            }
        }
    }
}
