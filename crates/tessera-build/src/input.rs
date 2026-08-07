//! Parquet input readers for the batch build.
//!
//! Two inputs, both read as record-batch streams so the reader's own memory stays bounded even
//! when the file does not (the *build's* in-memory item vector is the memory ceiling, not the
//! decoder's):
//!
//! * **points** — `entity_id` plus geometry, in any of three shapes and always yielding the same
//!   32-bit fixed-point form (see [`PointRow`]): explicit `x`/`y` columns; `morton` + `residual`,
//!   which the probe corpus writes and which carries the full 32 bits per axis; or a bare
//!   `morton` column, which carries 16 and is widened without pretending otherwise. See
//!   [`read_points`].
//! * **pairs** — the exploded `(entity_id, term_id)` relation.
//!
//! Both honour a `limit`: `entity_id < limit` selects a prefix of entity space, which is a
//! whole coherent corpus because entity IDs are append-only (I9, dataset §4.1). Row groups
//! whose statistics prove they hold no qualifying row are skipped outright — at 10⁹ items the
//! pairs relation is billions of rows and the prefix is a few hundred thousand.

use std::collections::HashMap;
use std::fs::File;
use std::ops::ControlFlow;
use std::path::Path;
use std::sync::mpsc;

use arrow::array::{Array, Float32Array, Float64Array, UInt32Array, UInt64Array};
use arrow::datatypes::{DataType, TimeUnit};
use arrow::record_batch::RecordBatch;
use parquet::arrow::arrow_reader::ParquetRecordBatchReaderBuilder;
use parquet::file::metadata::ParquetMetaData;
use parquet::file::statistics::Statistics;
use tessera_spatial::tiler::{ScalarType, ScalarValue};
use tessera_spatial::{fixed32, Bounds};

use crate::error::{BuildError, Result};

/// One input point: its source-corpus entity ID (which becomes the external ID) and geometry.
///
/// Geometry is carried **already quantised**, as 32-bit fixed point per axis against the build's
/// extent ([`tessera_spatial::fixed32`]), rather than as the coordinates the file held. Three
/// reasons, in the order they matter:
///
/// - It is the only form that can represent every input source without loss. A file holding
///   coordinates quantises exactly once, here; a file holding Morton codes plus residuals is
///   already in this form and is reassembled rather than converted; and a file holding bare
///   Morton codes carries 16 bits per axis, which widens into this form with zero residual and
///   no pretence that more precision exists.
/// - It is the same width as the `f32` pair it replaces, so the build's per-entity geometry
///   arrays do not grow. An `f64` pair would have carried the precision too, at twice the memory
///   on the one structure the build allocates per entity.
/// - The cell code and its residual both fall out by shift and mask
///   ([`tessera_spatial::split32`]), so no downstream stage re-quantises and none can disagree
///   with another about which cell a point belongs to.
#[derive(Debug, Clone, Copy)]
pub struct PointRow {
    pub source_id: u64,
    /// 32-bit fixed-point x against the build extent; `qx >> 16` is the cell.
    pub qx: u32,
    /// 32-bit fixed-point y against the build extent; `qy >> 16` is the cell.
    pub qy: u32,
}

/// The only extent under which the Morton input branch is meaningful: the grid's own
/// coordinates, `[0, 65536)` on both axes (contracts §2.5 — the grid is 2^16 x 2^16).
pub const IDENTITY_EXTENT: Bounds = Bounds {
    x_min: 0.0,
    x_max: 65536.0,
    y_min: 0.0,
    y_max: 65536.0,
};

/// Read `points`, keeping rows with `source_id < limit` when `limit` is `Some`.
///
/// Accepted schemas (checked in this order), all yielding [`PointRow`]'s 32-bit fixed point:
/// 1. `entity_id` + `x` + `y` — coordinates quantised against `extent`, the one place that
///    happens.
/// 2. `entity_id` + `morton` + `residual` — the two words are *reassembled*, not converted: the
///    file already holds the fixed-point position split across a cell code and a sub-cell
///    remainder, in this build's own axis convention. Full 32 bits per axis.
/// 3. `entity_id` + `morton` — 16 bits per axis, widened with a zero residual. The point sits at
///    its cell's origin because that is genuinely all the file says about it.
///
/// **Both Morton branches require [`IDENTITY_EXTENT`]**, where `cell(v) = v` for an integer
/// `v ≤ 65535`, and error otherwise. Under any other extent the cell indices would be
/// re-quantised as if they were coordinates in that extent's units, silently collapsing or
/// stretching the grid while `MANIFEST.json` went on declaring the extent the caller passed — a
/// bundle whose geometry and whose declared quantisation disagree. A corpus with real coordinates
/// ships `x`/`y` and takes branch 1.
pub fn read_points(path: &Path, extent: &Bounds, limit: Option<u64>) -> Result<Vec<PointRow>> {
    let mut out = Vec::new();
    scan_points(path, extent, limit, |row| {
        out.push(row);
        ControlFlow::Continue(())
    })?;
    Ok(out)
}

/// How many row groups each decoder worker claims, and the decoded-batch channel bound.
///
/// Decode is the expensive half of a scan (Snappy + delta unpacking); visiting is a few
/// instructions per row. So row groups are decoded on a small pool of worker threads and
/// *visited* on the calling thread, which keeps `visit` free of any `Send` requirement and the
/// resident set bounded by `DECODE_CHANNEL_BATCHES` decoded batches. **Rows arrive in no
/// particular order across row groups.** Every consumer is insensitive to arrival order: both
/// builds sort or group everything they read (the streaming build's pipeline argues this per
/// pass; the linear build sorts points by source id and groups pairs per entity before use).
const DECODE_WORKERS_MAX: usize = 6;
const DECODE_CHANNEL_BATCHES: usize = 16;

fn decode_worker_count(row_groups: usize) -> usize {
    std::thread::available_parallelism()
        .map(|n| n.get())
        .unwrap_or(4)
        .min(DECODE_WORKERS_MAX)
        .min(row_groups)
        .max(1)
}

/// The streaming form of [`read_points`]: calls `visit` once per selected row, holding only a
/// bounded number of decoded record batches. The batch build uses this so the points file —
/// 10⁹ rows in the probe corpus — can be traversed several times without ever being
/// materialised. Row groups are decoded in parallel; rows are therefore visited in **no
/// guaranteed order** (see [`decode_worker_count`]). `visit` returns [`ControlFlow`]:
/// `Break(())` stops the scan promptly (remaining rows are skipped and the decode workers wind
/// down) — the escape hatch for a caller whose own bookkeeping has already failed, so a fatal
/// error does not decode the rest of a multi-gigabyte file first.
pub fn scan_points<F: FnMut(PointRow) -> ControlFlow<()>>(
    path: &Path,
    extent: &Bounds,
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
        } else if schema.column_with_name("morton").is_some()
            && schema.column_with_name("residual").is_some()
        {
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
            vec!["entity_id", "morton", "residual"]
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

    // Project: the probe corpus carries columns this build has no use for, and at 10^9 rows
    // not decoding them is the difference between one pass and two. Each decode worker builds
    // its own `ProjectionMask` from these root indices against its own reader.
    let mut roots = Vec::with_capacity(wanted.len());
    for name in &wanted {
        roots.push(column_index(path, &schema, name)?);
    }

    let keep = prunable_row_groups(builder.metadata(), id_idx_in_file, limit);
    drop(builder);
    let geometry_kind = if wanted.contains(&"x") {
        GeometryKind::Xy
    } else if wanted.contains(&"residual") {
        GeometryKind::MortonResidual
    } else {
        GeometryKind::Morton
    };
    let workers = decode_worker_count(keep.len());
    let shards: Vec<Vec<usize>> = keep
        .chunks(keep.len().div_ceil(workers).max(1))
        .map(|c| c.to_vec())
        .collect();

    /// One decoded batch's columns, extracted on a worker thread.
    enum PointCols {
        Xy(Vec<u64>, Vec<f32>, Vec<f32>),
        /// Codes only: 16 bits per axis, all a bare `morton` column can carry.
        Morton(Vec<u64>, Vec<u64>),
        /// Codes plus sub-cell residuals: the full 32 bits per axis.
        MortonResidual(Vec<u64>, Vec<u64>, Vec<u64>),
    }

    let (tx, rx) =
        mpsc::sync_channel::<std::result::Result<PointCols, BuildError>>(DECODE_CHANNEL_BATCHES);
    std::thread::scope(|scope| {
        for shard in shards {
            let tx = tx.clone();
            let roots = roots.clone();
            scope.spawn(move || {
                let decode = |tx: &mpsc::SyncSender<_>| -> Result<()> {
                    let file = File::open(path).map_err(|e| BuildError::io(path, e))?;
                    let b = ParquetRecordBatchReaderBuilder::try_new(file)
                        .map_err(|e| BuildError::parquet(path, e))?;
                    let projection =
                        parquet::arrow::ProjectionMask::roots(b.parquet_schema(), roots);
                    let reader = b
                        .with_row_groups(shard)
                        .with_projection(projection)
                        .with_batch_size(65_536)
                        .build()
                        .map_err(|e| BuildError::parquet(path, e))?;
                    let projected = arrow::array::RecordBatchReader::schema(&reader);
                    let id_idx = column_index(path, &projected, "entity_id")?;
                    let geometry = match geometry_kind {
                        GeometryKind::Xy => Geometry::Xy(
                            column_index(path, &projected, "x")?,
                            column_index(path, &projected, "y")?,
                        ),
                        GeometryKind::Morton => {
                            Geometry::Morton(column_index(path, &projected, "morton")?)
                        }
                        GeometryKind::MortonResidual => Geometry::MortonResidual(
                            column_index(path, &projected, "morton")?,
                            column_index(path, &projected, "residual")?,
                        ),
                    };
                    for batch in reader {
                        let batch = batch.map_err(|e| BuildError::arrow(path, e))?;
                        let ids = read_u64_column(path, &batch, id_idx, "entity_id")?;
                        let cols = match geometry {
                            Geometry::Xy(xi, yi) => PointCols::Xy(
                                ids,
                                read_f32_column(path, &batch, xi, "x")?,
                                read_f32_column(path, &batch, yi, "y")?,
                            ),
                            Geometry::Morton(mi) => {
                                PointCols::Morton(ids, read_u64_column(path, &batch, mi, "morton")?)
                            }
                            Geometry::MortonResidual(mi, ri) => PointCols::MortonResidual(
                                ids,
                                read_u64_column(path, &batch, mi, "morton")?,
                                read_u64_column(path, &batch, ri, "residual")?,
                            ),
                        };
                        if tx.send(Ok(cols)).is_err() {
                            // The consumer went away (its own error path); stop quietly.
                            return Ok(());
                        }
                    }
                    Ok(())
                };
                if let Err(e) = decode(&tx) {
                    let _ = tx.send(Err(e));
                }
            });
        }
        drop(tx);

        let mut consume = || -> Result<()> {
            while let Ok(message) = rx.recv() {
                match message? {
                    PointCols::Xy(ids, xs, ys) => {
                        for i in 0..ids.len() {
                            if limit.is_some_and(|l| ids[i] >= l) {
                                continue;
                            }
                            // The one place a coordinate is quantised. `f32` widens to `f64`
                            // exactly, so this loses nothing the file had not already lost.
                            if visit(PointRow {
                                source_id: ids[i],
                                qx: fixed32(xs[i] as f64, extent.x_min, extent.x_max),
                                qy: fixed32(ys[i] as f64, extent.y_min, extent.y_max),
                            })
                            .is_break()
                            {
                                return Ok(());
                            }
                        }
                    }
                    PointCols::Morton(ids, codes) => {
                        for i in 0..ids.len() {
                            if limit.is_some_and(|l| ids[i] >= l) {
                                continue;
                            }
                            let code = narrow_code(path, codes[i], "morton")?;
                            let (cx, cy) = deinterleave(code);
                            // A bare `morton` column holds 16 bits per axis and no more, so the
                            // sub-cell position is *zero*, not unknown: the point sits at its
                            // cell's origin. Widening rather than inventing precision is what
                            // makes this branch honest, and it is why the source having no
                            // residual is a property of the corpus rather than a defect here.
                            if visit(PointRow {
                                source_id: ids[i],
                                qx: (cx as u32) << 16,
                                qy: (cy as u32) << 16,
                            })
                            .is_break()
                            {
                                return Ok(());
                            }
                        }
                    }
                    PointCols::MortonResidual(ids, codes, residuals) => {
                        for i in 0..ids.len() {
                            if limit.is_some_and(|l| ids[i] >= l) {
                                continue;
                            }
                            let code = narrow_code(path, codes[i], "morton")?;
                            let residual = narrow_code(path, residuals[i], "residual")?;
                            let (cx, cy) = deinterleave(code);
                            let (rx, ry) = deinterleave(residual);
                            // Reassembly, not conversion: the file already holds the 32-bit
                            // fixed-point position, split across two words in the same axis
                            // convention this build stores it in.
                            if visit(PointRow {
                                source_id: ids[i],
                                qx: ((cx as u32) << 16) | rx as u32,
                                qy: ((cy as u32) << 16) | ry as u32,
                            })
                            .is_break()
                            {
                                return Ok(());
                            }
                        }
                    }
                }
            }
            Ok(())
        };
        let result = consume();
        // Drop the receiver BEFORE the scope joins the workers: a worker blocked mid-send into
        // a full channel would otherwise deadlock the join when `consume` exited early.
        drop(rx);
        result
    })
}

/// Read `pairs` (`entity_id`, `term_id`), keeping rows with `entity_id < limit`, grouped into
/// each source entity's term list. Lists are returned sorted and deduplicated: the label set is
/// a *set*, and downstream (the signature key, the postings writer) depends on it being one.
pub fn read_pairs(path: &Path, limit: Option<u64>) -> Result<HashMap<u64, Vec<u64>>> {
    let mut grouped: HashMap<u64, Vec<u64>> = HashMap::new();
    scan_pairs(path, limit, |source_id, term| {
        grouped.entry(source_id).or_default().push(term);
        ControlFlow::Continue(())
    })?;
    for terms in grouped.values_mut() {
        terms.sort_unstable();
        terms.dedup();
    }
    Ok(grouped)
}

/// The streaming form of [`read_pairs`]: calls `visit(source_entity_id, source_term_id)` once
/// per selected row, holding only a bounded number of decoded record batches. Rows are **not**
/// grouped, sorted or deduplicated — that is the caller's business, and at 1.72 × 10⁹ pairs it
/// is the difference between a bounded traversal and a 70 GB `HashMap`. Row groups are decoded
/// in parallel; rows are therefore visited in **no guaranteed order** (see
/// [`decode_worker_count`]), and `visit`'s `Break` stops the scan promptly (see
/// [`scan_points`]).
pub fn scan_pairs<F: FnMut(u64, u64) -> ControlFlow<()>>(
    path: &Path,
    limit: Option<u64>,
    mut visit: F,
) -> Result<()> {
    let file = File::open(path).map_err(|e| BuildError::io(path, e))?;
    let builder =
        ParquetRecordBatchReaderBuilder::try_new(file).map_err(|e| BuildError::parquet(path, e))?;
    let schema = builder.schema().clone();
    let id_idx = column_index(path, &schema, "entity_id")?;
    let term_idx = column_index(path, &schema, "term_id")?;

    let keep = prunable_row_groups(builder.metadata(), id_idx, limit);
    drop(builder);
    let workers = decode_worker_count(keep.len());
    let shards: Vec<Vec<usize>> = keep
        .chunks(keep.len().div_ceil(workers).max(1))
        .map(|c| c.to_vec())
        .collect();

    let (tx, rx) = mpsc::sync_channel::<std::result::Result<(Vec<u64>, Vec<u64>), BuildError>>(
        DECODE_CHANNEL_BATCHES,
    );
    std::thread::scope(|scope| {
        for shard in shards {
            let tx = tx.clone();
            scope.spawn(move || {
                let decode = |tx: &mpsc::SyncSender<_>| -> Result<()> {
                    let file = File::open(path).map_err(|e| BuildError::io(path, e))?;
                    let b = ParquetRecordBatchReaderBuilder::try_new(file)
                        .map_err(|e| BuildError::parquet(path, e))?;
                    let reader = b
                        .with_row_groups(shard)
                        .with_batch_size(65_536)
                        .build()
                        .map_err(|e| BuildError::parquet(path, e))?;
                    for batch in reader {
                        let batch = batch.map_err(|e| BuildError::arrow(path, e))?;
                        let ids = read_u64_column(path, &batch, id_idx, "entity_id")?;
                        let terms = read_u64_column(path, &batch, term_idx, "term_id")?;
                        if tx.send(Ok((ids, terms))).is_err() {
                            return Ok(());
                        }
                    }
                    Ok(())
                };
                if let Err(e) = decode(&tx) {
                    let _ = tx.send(Err(e));
                }
            });
        }
        drop(tx);

        let mut consume = || -> Result<()> {
            while let Ok(message) = rx.recv() {
                let (ids, terms) = message?;
                for i in 0..ids.len() {
                    if limit.is_some_and(|l| ids[i] >= l) {
                        continue;
                    }
                    if visit(ids[i], terms[i]).is_break() {
                        return Ok(());
                    }
                }
            }
            Ok(())
        };
        let result = consume();
        // See scan_points: the receiver must drop before the scope joins the workers.
        drop(rx);
        result
    })
}

/// The points file's total row count, from parquet metadata alone — no decode.
///
/// Exact for an unfiltered scan: [`scan_points`] visits every row when there is no limit
/// (the extent quantises, it never filters). With a limit the selected count is data-dependent
/// and only a counting scan can establish it. The count is advisory (it sizes an allocation);
/// every correctness property downstream is established from the rows actually read.
pub fn count_point_rows(path: &Path) -> Result<u64> {
    let file = File::open(path).map_err(|e| BuildError::io(path, e))?;
    let builder =
        ParquetRecordBatchReaderBuilder::try_new(file).map_err(|e| BuildError::parquet(path, e))?;
    Ok(builder.metadata().file_metadata().num_rows().max(0) as u64)
}

#[derive(Debug, Clone, Copy)]
enum Geometry {
    Xy(usize, usize),
    Morton(usize),
    MortonResidual(usize, usize),
}

/// Which geometry schema the points file offers, decided once from the file's columns and then
/// carried to every decode worker (each resolves its own column indices against its own reader).
#[derive(Clone, Copy)]
enum GeometryKind {
    Xy,
    Morton,
    MortonResidual,
}

/// Narrow a `u64` column value to the `u32` a Morton or residual word must fit in, as a typed
/// error rather than a truncation — a truncated code is a different position, silently.
fn narrow_code(path: &Path, value: u64, name: &str) -> Result<u32> {
    u32::try_from(value).map_err(|_| BuildError::Schema {
        path: path.to_path_buf(),
        detail: format!("{name} value {value} does not fit in u32"),
    })
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
            // Arrow's default cast is *safe*: a negative value becomes a null, and reading
            // `.values()` underneath a null yields an arbitrary id silently. Nulls were checked
            // on the source column above; check again after the cast so a negative id is a
            // schema error, never a wrong id.
            if cast.null_count() > 0 {
                return Err(BuildError::Schema {
                    path: path.to_path_buf(),
                    detail: format!("column '{name}' contains negative values"),
                });
            }
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

/// Read a vocabulary file: `(key, code)` plus an optional `label` (§4.4).
///
/// **Parquet, like every other build input**, so a 400-value published vocabulary is the same
/// kind of artifact as the points and pairs files and needs no second reader.
///
/// A `gate` column is **refused rather than ignored** (⊘, §3.8): an explicit gate label replaces
/// membership-derivation for its value, which is an authorisation statement, and a build that
/// silently dropped it would produce a bundle whose vocabulary is more visible than its author
/// declared. There is no vocabulary-visibility evaluation yet to honour it, so refusing is the
/// only answer that does not manufacture an assurance.
pub fn read_vocabulary_file(path: &Path, attribute: &str) -> Result<crate::schema::ValueSet> {
    use arrow::array::StringArray;

    let file = File::open(path).map_err(|e| BuildError::io(path, e))?;
    let builder =
        ParquetRecordBatchReaderBuilder::try_new(file).map_err(|e| BuildError::parquet(path, e))?;
    let schema = builder.schema().clone();
    if schema.column_with_name("gate").is_some() {
        return Err(crate::schema::schema_error(format!(
            "attribute '{attribute}': the vocabulary at {} carries a `gate` column, which is \
             specified and not built (per-point-attributes §3.8). An explicit gate label replaces \
             membership-derivation for its value — an authorisation statement — and nothing \
             evaluates one yet. Refused rather than dropped: a dropped gate is a value more \
             visible than its author declared",
            path.display()
        )));
    }
    let key_idx = column_index(path, &schema, "key")?;
    let code_idx = column_index(path, &schema, "code")?;
    let label_idx = schema.column_with_name("label").map(|(i, _)| i);

    let reader = builder
        .build()
        .map_err(|e| BuildError::parquet(path, e))?;

    let mut set = crate::schema::ValueSet::default();
    for batch in reader {
        let batch = batch.map_err(|e| BuildError::arrow(path, e))?;
        let keys = batch
            .column(key_idx)
            .as_any()
            .downcast_ref::<StringArray>()
            .ok_or_else(|| BuildError::Schema {
                path: path.to_path_buf(),
                detail: "vocabulary column 'key' must be utf8".into(),
            })?;
        let code_values = read_u64_column(path, &batch, code_idx, "code")?;
        let label_values = match label_idx {
            Some(idx) => Some(
                batch
                    .column(idx)
                    .as_any()
                    .downcast_ref::<StringArray>()
                    .ok_or_else(|| BuildError::Schema {
                        path: path.to_path_buf(),
                        detail: "vocabulary column 'label' must be utf8".into(),
                    })?
                    .clone(),
            ),
            None => None,
        };
        for (row, raw) in code_values.iter().enumerate() {
            let key = keys.value(row).to_string();
            let code = u32::try_from(*raw).map_err(|_| {
                crate::schema::schema_error(format!(
                    "attribute '{attribute}': value '{key}' has code {raw}, which is not a u32"
                ))
            })?;
            // A duplicate key here is a duplicate *code assignment*, which the caller's file
            // decides silently by row order unless it is refused. `check_codes` catches two keys
            // at one code; this catches one key at two.
            if let Some(previous) = set.codes.insert(key.clone(), code) {
                return Err(crate::schema::schema_error(format!(
                    "attribute '{attribute}': the vocabulary at {} lists key '{key}' twice, at \
                     codes {previous} and {code}. Which one every row carrying '{key}' would \
                     mean is decided by row order, so it is refused",
                    path.display()
                )));
            }
            if let Some(values) = &label_values {
                set.labels.insert(key, values.value(row).to_string());
            }
        }
    }
    // `reserved` has no file spelling: a tombstone belongs in the reviewed schema artifact rather
    // than in a regenerable data file, on §3.4's argument that a re-sorted or regenerated
    // vocabulary file must not be able to change what a stored code means.
    Ok(set)
}

/// Stream the declared attribute columns, calling `visit(entity_id, values)` once per selected
/// row with the values in **declared order** — the order `columns.arrow`'s tail is written and
/// read back in.
///
/// **A second pass over the points file rather than a widening of [`scan_points`].** [`PointRow`]
/// is a 16-byte `Copy` struct held one per entity by both builds, and its doc argues that width;
/// a variable-length attribute tail hung off it would make the build's one per-entity structure
/// grow with the schema. The two passes are independent, and this one is single-threaded because
/// an attribute column is 1–8 bytes against geometry's decode cost — the parallel decode
/// [`scan_points`] needs buys nothing here.
///
/// Category keys are mapped to codes through `vocabularies`; a key the vocabulary does not
/// declare is a **build failure** naming the column and the key, per §5's declare-then-use rule.
/// A row whose category column is null carries [`crate::schema::ABSENT_CODE`].
pub fn scan_attributes<F: FnMut(u64, &[ScalarValue])>(
    path: &Path,
    schema_decl: &crate::schema::Schema,
    limit: Option<u64>,
    mut visit: F,
) -> Result<()> {
    use arrow::array::StringArray;

    if schema_decl.is_empty() {
        return Ok(());
    }
    let file = File::open(path).map_err(|e| BuildError::io(path, e))?;
    let builder =
        ParquetRecordBatchReaderBuilder::try_new(file).map_err(|e| BuildError::parquet(path, e))?;
    let file_schema = builder.schema().clone();

    let mut roots = vec![column_index(path, &file_schema, "entity_id")?];
    for attribute in &schema_decl.attributes {
        roots.push(
            file_schema
                .column_with_name(&attribute.name)
                .map(|(i, _)| i)
                .ok_or_else(|| BuildError::Schema {
                    path: path.to_path_buf(),
                    detail: format!(
                        "the schema declares attribute '{}', which this points file has no \
                         column for. A declared column the data lacks would otherwise be written \
                         as the absent sentinel for every row — a column that cost its width to \
                         say nothing",
                        attribute.name
                    ),
                })?,
        );
    }
    let projection =
        parquet::arrow::ProjectionMask::roots(builder.parquet_schema(), roots.clone());
    let reader = builder
        .with_projection(projection)
        .with_batch_size(65_536)
        .build()
        .map_err(|e| BuildError::parquet(path, e))?;
    let projected = arrow::array::RecordBatchReader::schema(&reader);
    let id_idx = column_index(path, &projected, "entity_id")?;
    let attribute_idx: Vec<usize> = schema_decl
        .attributes
        .iter()
        .map(|a| column_index(path, &projected, &a.name))
        .collect::<Result<_>>()?;

    let mut row_values: Vec<ScalarValue> = Vec::with_capacity(schema_decl.attributes.len());
    for batch in reader {
        let batch = batch.map_err(|e| BuildError::arrow(path, e))?;
        let ids = read_u64_column(path, &batch, id_idx, "entity_id")?;
        for (row, &entity_id) in ids.iter().enumerate() {
            if limit.is_some_and(|l| entity_id >= l) {
                continue;
            }
            row_values.clear();
            for (attribute, &idx) in schema_decl.attributes.iter().zip(&attribute_idx) {
                let column = batch.column(idx);
                let value = match &attribute.vocabulary {
                    Some(vocabulary) => {
                        // A category arrives as its *key*, never as a code: §3.1 — the key in the
                        // row is not the display name, and the code is assigned once and pinned,
                        // so a data file supplying codes directly would be a second place codes
                        // are decided.
                        let keys = column
                            .as_any()
                            .downcast_ref::<StringArray>()
                            .ok_or_else(|| BuildError::Schema {
                                path: path.to_path_buf(),
                                detail: format!(
                                    "attribute '{}' is a category, so its column must hold value \
                                     keys (utf8); this file holds {:?}. A category's code is \
                                     assigned once from the vocabulary and never re-derived from \
                                     the data (per-point-attributes §3.4)",
                                    attribute.name,
                                    column.data_type()
                                ),
                            })?;
                        let code = if keys.is_null(row) {
                            crate::schema::ABSENT_CODE
                        } else {
                            let key = keys.value(row);
                            schema_decl.vocabularies[vocabulary]
                                .code_of(key)
                                .ok_or_else(|| {
                                    crate::schema::schema_error(format!(
                                        "attribute '{}': the points file carries value '{key}', \
                                         which the declared vocabulary does not list. Under \
                                         `vocabulary = \"declared\"` there is no auto-mint: a \
                                         category carries properties and a visibility \
                                         consequence, so a typo must not create one \
                                         (per-point-attributes §5)",
                                        attribute.name
                                    ))
                                })?
                        };
                        code_as(attribute.ty, code)
                    }
                    None => plain_scalar(path, column, row, attribute)?,
                };
                row_values.push(value);
            }
            visit(entity_id, &row_values);
        }
    }
    Ok(())
}

/// A vocabulary code at the column's declared width. Every code reaching here was checked
/// against [`ScalarType::max_code`] at parse, so the narrowing cannot lose a value.
fn code_as(ty: ScalarType, code: u32) -> ScalarValue {
    match ty {
        ScalarType::U8 => ScalarValue::U8(code as u8),
        ScalarType::U16 => ScalarValue::U16(code as u16),
        _ => ScalarValue::U32(code),
    }
}

/// One non-category attribute's value, at the column's declared type.
///
/// **Null becomes the type's zero**, matching a category's *absent* sentinel: `columns.arrow` is
/// contractually non-nullable (R4) and the reader refuses a nullable column outright, so there is
/// no third state to carry. For a numeric attribute that makes zero ambiguous between "absent"
/// and "zero", which is why §3's first-class case is the category — where code 0 means absent and
/// nothing else.
fn plain_scalar(
    path: &Path,
    column: &arrow::array::ArrayRef,
    row: usize,
    attribute: &crate::schema::Attribute,
) -> Result<ScalarValue> {
    use arrow::array::{Float32Array, Float64Array, UInt64Array};
    let mismatch = |found: &DataType| BuildError::Schema {
        path: path.to_path_buf(),
        detail: format!(
            "attribute '{}' is declared '{}', but the points file holds {found:?}. The width is \
             baked into every row and changing it rewrites the corpus (per-point-attributes \
             §2.2), so it is taken from the declaration and the data must match it.{}",
            attribute.name,
            attribute.ty.arrow_type_name(),
            match found {
                // The one mismatch a caller is likely to hit while doing everything right: a
                // timestamp column is an `i64` and reads as one, but only in microseconds.
                DataType::Timestamp(unit, _) if *unit != TimeUnit::Microsecond => format!(
                    " This is a timestamp in {unit:?}, and only microseconds are accepted: \
                     nothing records a unit, so accepting two would store incomparable numbers \
                     under one declaration. Cast the column to timestamp[us] (or to a plain i64 \
                     of whatever unit you mean) before building"
                ),
                _ => String::new(),
            }
        ),
    };
    if column.is_null(row) {
        return Ok(match attribute.ty {
            ScalarType::U8 => ScalarValue::U8(0),
            ScalarType::U16 => ScalarValue::U16(0),
            ScalarType::U32 => ScalarValue::U32(0),
            ScalarType::U64 => ScalarValue::U64(0),
            ScalarType::I64 => ScalarValue::I64(0),
            ScalarType::F32 => ScalarValue::F32(0.0),
            ScalarType::Utf8 => ScalarValue::Utf8(String::new()),
        });
    }
    let any = column.as_any();
    // Each arm accepts the declared type and the wider source types that carry it losslessly —
    // a `u8` attribute read from a parquet `u32` column, which is how most tooling writes small
    // integers — and refuses anything else rather than truncating.
    Ok(match attribute.ty {
        ScalarType::U8 => ScalarValue::U8(narrow(
            read_integer(any, column.data_type()).ok_or_else(|| mismatch(column.data_type()))?
                [row],
            u8::MAX as i64,
            attribute,
        )? as u8),
        ScalarType::U16 => ScalarValue::U16(narrow(
            read_integer(any, column.data_type()).ok_or_else(|| mismatch(column.data_type()))?
                [row],
            u16::MAX as i64,
            attribute,
        )? as u16),
        ScalarType::U32 => ScalarValue::U32(narrow(
            read_integer(any, column.data_type()).ok_or_else(|| mismatch(column.data_type()))?
                [row],
            u32::MAX as i64,
            attribute,
        )? as u32),
        ScalarType::U64 => {
            if let Some(a) = any.downcast_ref::<UInt64Array>() {
                ScalarValue::U64(a.value(row))
            } else {
                let v = read_integer(any, column.data_type())
                    .ok_or_else(|| mismatch(column.data_type()))?[row];
                ScalarValue::U64(u64::try_from(v).map_err(|_| mismatch(column.data_type()))?)
            }
        }
        ScalarType::I64 => ScalarValue::I64(
            read_integer(any, column.data_type()).ok_or_else(|| mismatch(column.data_type()))?
                [row],
        ),
        // **`f64` rounds to `f32` and is not refused, unlike a too-wide integer.** The asymmetry
        // is deliberate and is easy to read as an oversight, so: narrowing an integer produces a
        // *different value* — a `u8` given 300 stores 44 — whereas narrowing a float produces the
        // nearest value the declared width can hold, which is what declaring `f32` asks for. The
        // caller chose four bytes per row; rounding is that choice being honoured, not a silent
        // failure to honour it.
        //
        // It is worth knowing that most parquet writers emit `double` by default, so a caller who
        // wanted full precision and declared `f32` out of habit gets rounding without being told.
        // The remedy is a declarable `f64`, which does not exist — there is no way to ask for
        // eight-byte floats today.
        ScalarType::F32 => {
            if let Some(a) = any.downcast_ref::<Float32Array>() {
                ScalarValue::F32(a.value(row))
            } else if let Some(a) = any.downcast_ref::<Float64Array>() {
                ScalarValue::F32(a.value(row) as f32)
            } else {
                return Err(mismatch(column.data_type()));
            }
        }
        ScalarType::Utf8 => {
            // Unreachable: `render` on `utf8` is refused at parse (§4.3). Kept as an error rather
            // than an `unreachable!` so that lifting that refusal cannot land on a panic.
            return Err(mismatch(column.data_type()));
        }
    })
}

/// Any integer parquet column as `i64`, or `None` if it is not an integer column at all.
fn read_integer(any: &dyn std::any::Any, ty: &DataType) -> Option<Vec<i64>> {
    use arrow::array::{Int32Array, Int64Array, UInt16Array, UInt32Array, UInt64Array, UInt8Array};
    Some(match ty {
        DataType::UInt8 => any
            .downcast_ref::<UInt8Array>()?
            .values()
            .iter()
            .map(|v| *v as i64)
            .collect(),
        DataType::UInt16 => any
            .downcast_ref::<UInt16Array>()?
            .values()
            .iter()
            .map(|v| *v as i64)
            .collect(),
        DataType::UInt32 => any
            .downcast_ref::<UInt32Array>()?
            .values()
            .iter()
            .map(|v| *v as i64)
            .collect(),
        DataType::UInt64 => any
            .downcast_ref::<UInt64Array>()?
            .values()
            .iter()
            .map(|v| *v as i64)
            .collect(),
        DataType::Int32 => any
            .downcast_ref::<Int32Array>()?
            .values()
            .iter()
            .map(|v| *v as i64)
            .collect(),
        DataType::Int64 => any.downcast_ref::<Int64Array>()?.values().to_vec(),
        // **Microseconds only, and the other units are refused rather than accepted.** A timestamp
        // is an `i64` of its unit, and nothing records which unit: `MANIFEST.declared_scalars`
        // says `i64`. So a build that silently took milliseconds from one source and microseconds
        // from another would store two incomparable numbers under one declaration, and the
        // difference would surface as dates a thousandfold wrong rather than as an error.
        //
        // Normalising here was the alternative and is worse: it would rewrite the caller's values
        // on a rule they never stated. Refusing tells them to cast, which is a decision they make
        // once, visibly, in their own pipeline.
        //
        // The arm previously matched `Timestamp(_, _)` and then downcast only to
        // `TimestampMicrosecondArray`, so every other unit fell through to a type-mismatch error
        // complaining about a type the caller had declared correctly.
        DataType::Timestamp(TimeUnit::Microsecond, _) => any
            .downcast_ref::<arrow::array::TimestampMicrosecondArray>()?
            .values()
            .to_vec(),
        _ => return None,
    })
}

/// A value that must fit the declared width, refused rather than truncated.
///
/// **The refusal is the point.** A `u8` category column whose data carries 300 is a build that
/// would otherwise write 44 — a different value, in a column whose width cannot be changed
/// without rewriting the corpus, with nothing downstream able to notice.
fn narrow(value: i64, max: i64, attribute: &crate::schema::Attribute) -> Result<i64> {
    if value < 0 || value > max {
        return Err(crate::schema::schema_error(format!(
            "attribute '{}': the points file carries {value}, which does not fit its declared \
             '{}' (0..={max}). Refused rather than truncated — the width is baked into every row \
             and the remedy is a rebuild at a wider declaration (per-point-attributes §3.6)",
            attribute.name,
            attribute.ty.arrow_type_name()
        )));
    }
    Ok(value)
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
