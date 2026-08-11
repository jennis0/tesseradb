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
use tessera_store::vocabulary::VocabularyMinter;

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

    let reader = builder.build().map_err(|e| BuildError::parquet(path, e))?;

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
/// Category keys are mapped to codes against `schema_decl`'s compiled vocabularies. Under a
/// **declared** vocabulary an unknown key is a **build failure** naming the column and the key,
/// per §5's declare-then-use rule. Under a **discovered** one, `minters` supplies a live
/// [`VocabularyMinter`] per vocabulary — seeded from whatever the schema already pins — and this
/// function mints a code for every key the batch introduces that the minter does not yet carry
/// (§3.4). Minting is a **batch-level pre-pass, not per row**: [`BatchColumn::decode`] collects
/// the distinct keys of one Arrow batch, mints any novel ones once each, and only then maps every
/// row through the now-complete lookup — never once per row, which is both the performance point
/// and the reason [`BatchColumn::value`] stays a pure positional lookup over already-resolved
/// data. A row whose category column is null carries [`crate::schema::ABSENT_CODE`], for either
/// kind.
///
/// `minters` is threaded through rather than owned here so the caller can hand its final state —
/// every binding this scan minted, on top of whatever the schema seeded it with — to the manifest
/// writer once the whole scan (there is exactly one, per build) has completed.
pub fn scan_attributes<F: FnMut(u64, &[ScalarValue])>(
    path: &Path,
    schema_decl: &crate::schema::Schema,
    minters: &mut HashMap<String, VocabularyMinter>,
    limit: Option<u64>,
    mut visit: F,
) -> Result<()> {
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
    let projection = parquet::arrow::ProjectionMask::roots(builder.parquet_schema(), roots.clone());
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

        // **Decoded once per batch, not once per row.** An earlier revision called a
        // whole-column converter from inside the row loop, so a 65,536-row batch decoded its
        // integer columns 65,536 times — quadratic in the batch size, and invisible at the scale
        // a test uses. A discovered category's mint pre-pass rides the same discipline: minting
        // is per distinct key in the batch, decided here, not per row.
        let mut decoded: Vec<BatchColumn> = Vec::with_capacity(schema_decl.attributes.len());
        for (attribute, &idx) in schema_decl.attributes.iter().zip(&attribute_idx) {
            decoded.push(BatchColumn::decode(
                path,
                batch.column(idx),
                attribute,
                minters,
            )?);
        }

        for (row, &entity_id) in ids.iter().enumerate() {
            if limit.is_some_and(|l| entity_id >= l) {
                continue;
            }
            row_values.clear();
            for (attribute, column) in schema_decl.attributes.iter().zip(&decoded) {
                row_values.push(column.value(row, attribute, schema_decl)?);
            }
            visit(entity_id, &row_values);
        }
    }
    Ok(())
}

/// One batch's worth of a declared column, decoded to the shape the row loop indexes, together
/// with the source's own record of which rows carry nothing.
///
/// **The null buffer is kept rather than dropped, and that is the whole of decision 0062's build
/// half.** Every numeric variant below is built from `values()`, which is the values buffer alone:
/// a null slot holds whatever is in it, which for every Arrow numeric is `0`. Reading that back
/// stores an item with no score as one scoring zero — present, and indistinguishable from a real
/// zero — so it matches a range containing zero, which is a wrong answer rather than an absent
/// feature. `NullBuffer` is an `Arc`'d bitmap, so carrying it costs a clone of a pointer.
///
/// The two families that already had somewhere to put absence keep doing so and do not consult
/// this: a category spends the reserved code 0, and `Text` carries its own null through to
/// `ScalarValue::Null`.
struct BatchColumn {
    nulls: Option<arrow::buffer::NullBuffer>,
    values: BatchValues,
}

/// The *source* shapes, not the declared types: several declarations read from one shape (every
/// integer width from `Ints`), and the declaration decides what a row's value becomes, not what the
/// file holds.
enum BatchValues {
    /// Category keys under a **declared** vocabulary, resolved per row: `value` looks each key up
    /// against `schema_decl` and refuses an unknown one (§5's declare-then-use).
    Keys(arrow::array::StringArray),
    /// Category codes under a **discovered** vocabulary, already resolved by the batch-level mint
    /// pre-pass in `decode` — every key this batch carries was minted or found bound before this
    /// variant exists, so `value` is a pure index, exactly as every other variant's is.
    Discovered(Vec<u32>),
    Bool(arrow::array::BooleanArray),
    /// Every integer column, widened to `i64` once. `narrow` puts each value back inside its
    /// declared width, refusing rather than truncating.
    Ints(Vec<i64>),
    /// A `u64` column read from a `u64` source, kept unwidened.
    ///
    /// **The one integer that cannot go through `Ints`.** Widening to `i64` is lossless for every
    /// other width, but a `u64` above `i64::MAX` — an ordinary hash, which is what a stable
    /// per-item identifier usually is — reads as negative and is then refused as out of range.
    /// Caught by the packed fixture on its first build, where `id_hash` is a blake2b digest and
    /// half of them have the high bit set.
    U64(Vec<u64>),
    F32(Vec<f32>),
    F64(Vec<f64>),
    /// A per-item string, for a `filter`-only `utf8` column.
    ///
    /// **Not a category**, and the distinction is the data model rather than the encoding: a
    /// category's value is a vocabulary entry with an identity, a pinned code and a lifecycle,
    /// existing independently of any row; a string is row data whose visibility is the visibility
    /// of the rows carrying it. So this variant resolves nothing against a vocabulary and mints
    /// nothing — the bytes are the value (per-point-attributes; filter-index §2.3).
    Text(arrow::array::StringArray),
}

impl BatchColumn {
    fn decode(
        path: &Path,
        column: &arrow::array::ArrayRef,
        attribute: &crate::schema::Attribute,
        minters: &mut HashMap<String, VocabularyMinter>,
    ) -> Result<Self> {
        let nulls = column.nulls().cloned();
        Ok(BatchColumn {
            nulls,
            values: Self::decode_values(path, column, attribute, minters)?,
        })
    }

    fn decode_values(
        path: &Path,
        column: &arrow::array::ArrayRef,
        attribute: &crate::schema::Attribute,
        minters: &mut HashMap<String, VocabularyMinter>,
    ) -> Result<BatchValues> {
        let mismatch = || BuildError::Schema {
            path: path.to_path_buf(),
            detail: format!(
                "attribute '{}' is declared '{}', but the points file holds {:?}. The width is \
                 baked into every row and changing it rewrites the corpus (per-point-attributes \
                 §2.2), so it is taken from the declaration and the data must match it.{}",
                attribute.name,
                attribute.ty.arrow_type_name(),
                column.data_type(),
                match column.data_type() {
                    // The one mismatch a caller is likely to hit while doing everything right: a
                    // timestamp column is an `i64` and reads as one, but only in microseconds.
                    DataType::Timestamp(unit, _) if *unit != TimeUnit::Microsecond => format!(
                        " This is a timestamp in {unit:?}, and only microseconds are accepted: \
                         nothing records a unit, so accepting two would store incomparable \
                         numbers under one declaration. Cast the column to timestamp[us] (or to \
                         a plain i64 of whatever unit you mean) before building"
                    ),
                    _ => String::new(),
                }
            ),
        };
        let any = column.as_any();
        if let Some(vocabulary) = &attribute.vocabulary {
            // A category arrives as its *key*, never as a code: §3.1 — the key in the row is not
            // the display name, and the code is assigned once and pinned, so a data file
            // supplying codes directly would be a second place codes are decided.
            let keys = any
                .downcast_ref::<arrow::array::StringArray>()
                .ok_or_else(|| BuildError::Schema {
                    path: path.to_path_buf(),
                    detail: format!(
                        "attribute '{}' is a category, so its column must hold value keys (utf8); \
                         this file holds {:?}. A category's code is assigned once from the \
                         vocabulary and never re-derived from the data (per-point-attributes §3.4)",
                        attribute.name,
                        column.data_type()
                    ),
                })?;
            return match attribute.vocabulary_kind {
                Some(crate::schema::VocabularyKind::Discovered) => {
                    let minter = minters.get_mut(vocabulary).unwrap_or_else(|| {
                        panic!(
                            "'{vocabulary}' is discovered, so `Schema::discovered_minters` must \
                             have seeded it before this scan began"
                        )
                    });
                    Ok(BatchValues::Discovered(mint_batch(
                        keys, minter, attribute,
                    )?))
                }
                // Declared (or a `values_of` share of one): resolved per row in `value`,
                // unchanged from the declare-then-use rule.
                _ => Ok(BatchValues::Keys(keys.clone())),
            };
        }
        Ok(match attribute.ty {
            ScalarType::Bool => BatchValues::Bool(
                any.downcast_ref::<arrow::array::BooleanArray>()
                    .ok_or_else(mismatch)?
                    .clone(),
            ),
            // **`f32` accepts `f64` and rounds; `f64` accepts `f32` and widens.** Neither is the
            // refusal an out-of-range integer gets, and the asymmetry is deliberate: narrowing an
            // integer produces a *different* value (a `u8` given 300 stores 44), narrowing a float
            // produces the nearest value the declared width holds, which is what declaring `f32`
            // asks for. A caller who wants the precision declares `f64`.
            ScalarType::F32 => {
                BatchValues::F32(if let Some(a) = any.downcast_ref::<Float32Array>() {
                    a.values().to_vec()
                } else if let Some(a) = any.downcast_ref::<Float64Array>() {
                    a.values().iter().map(|v| *v as f32).collect()
                } else {
                    return Err(mismatch());
                })
            }
            ScalarType::F64 => {
                BatchValues::F64(if let Some(a) = any.downcast_ref::<Float64Array>() {
                    a.values().to_vec()
                } else if let Some(a) = any.downcast_ref::<Float32Array>() {
                    a.values().iter().map(|v| *v as f64).collect()
                } else {
                    return Err(mismatch());
                })
            }
            // Reached only by a `filter`-only column: `render` on `utf8` is still refused at parse
            // (§4.3 — a non-fixed-width type in the hot column), but a filter column lives in
            // entity space and costs the hot column nothing.
            ScalarType::Utf8 => BatchValues::Text(
                any.downcast_ref::<arrow::array::StringArray>()
                    .ok_or_else(mismatch)?
                    .clone(),
            ),
            // A `u64` declaration over a `u64` source keeps the full range; every other
            // combination widens, which is lossless for it.
            ScalarType::U64 if any.is::<UInt64Array>() => BatchValues::U64(
                any.downcast_ref::<UInt64Array>()
                    .expect("checked by is::<>")
                    .values()
                    .to_vec(),
            ),
            _ => BatchValues::Ints(read_integer(any, column.data_type()).ok_or_else(mismatch)?),
        })
    }

    fn value(
        &self,
        row: usize,
        attribute: &crate::schema::Attribute,
        schema_decl: &crate::schema::Schema,
    ) -> Result<ScalarValue> {
        // **Absence, for every family that has no in-band marker.** The two that do are handled in
        // their own arms below and never reach this: a category spends the reserved code 0, and
        // `Text` carries the source's null through itself. Everything else is a number, whose every
        // bit pattern is a legal value — so the source's null buffer is the only thing that
        // distinguishes "carries no score" from "scores zero", and reading `values()` past it
        // silently makes the two the same (decision 0062).
        if self.nulls.as_ref().is_some_and(|n| n.is_null(row))
            && !matches!(
                self.values,
                BatchValues::Keys(_) | BatchValues::Discovered(_) | BatchValues::Text(_)
            )
        {
            return Ok(ScalarValue::Null);
        }
        Ok(match &self.values {
            // A null is *absent*; the empty string is a value the caller supplied. Carried apart
            // rather than folded together, because a string has no spare in-band value to spend on
            // absence the way a category spends code 0 — and folding them would report an item as
            // matching a value it does not have.
            BatchValues::Text(values) => {
                if values.is_null(row) {
                    ScalarValue::Null
                } else {
                    ScalarValue::Utf8(values.value(row).to_string())
                }
            }
            BatchValues::Keys(keys) => {
                let code = if keys.is_null(row) {
                    crate::schema::ABSENT_CODE
                } else {
                    let key = keys.value(row);
                    let vocabulary = attribute
                        .vocabulary
                        .as_ref()
                        .expect("a Keys column belongs to a category");
                    schema_decl.vocabularies[vocabulary]
                        .code_of(key)
                        .ok_or_else(|| {
                            crate::schema::schema_error(format!(
                                "attribute '{}': the points file carries value '{key}', which the \
                                 declared vocabulary does not list. Under \
                                 `vocabulary = \"declared\"` there is no auto-mint: a category \
                                 carries properties and a visibility consequence, so a typo must \
                                 not create one (per-point-attributes §5)",
                                attribute.name
                            ))
                        })?
                };
                code_as(attribute.ty, code)
            }
            // Already resolved by `decode`'s mint pre-pass — a pure index, like every other
            // variant here, and no lookup against `schema_decl` at all.
            BatchValues::Discovered(codes) => code_as(attribute.ty, codes[row]),
            BatchValues::Bool(values) => ScalarValue::Bool(values.value(row)),
            BatchValues::U64(values) => ScalarValue::U64(values[row]),
            BatchValues::F32(values) => ScalarValue::F32(values[row]),
            BatchValues::F64(values) => ScalarValue::F64(values[row]),
            BatchValues::Ints(values) => {
                let v = values[row];
                let range = |min: i64, max: i64| narrow(v, min, max, attribute);
                match attribute.ty {
                    ScalarType::U8 => ScalarValue::U8(range(0, u8::MAX as i64)? as u8),
                    ScalarType::U16 => ScalarValue::U16(range(0, u16::MAX as i64)? as u16),
                    ScalarType::U32 => ScalarValue::U32(range(0, u32::MAX as i64)? as u32),
                    ScalarType::U64 => ScalarValue::U64(range(0, i64::MAX)? as u64),
                    ScalarType::I8 => ScalarValue::I8(range(i8::MIN as i64, i8::MAX as i64)? as i8),
                    ScalarType::I16 => {
                        ScalarValue::I16(range(i16::MIN as i64, i16::MAX as i64)? as i16)
                    }
                    ScalarType::I32 => {
                        ScalarValue::I32(range(i32::MIN as i64, i32::MAX as i64)? as i32)
                    }
                    ScalarType::I64 => ScalarValue::I64(v),
                    ScalarType::TimestampUs => ScalarValue::TimestampUs(v),
                    other => {
                        return Err(BuildError::Invalid(format!(
                            "attribute '{}': '{}' is not an integer declaration",
                            attribute.name,
                            other.arrow_type_name()
                        )))
                    }
                }
            }
        })
    }
}

/// A vocabulary code at the column's declared width. A declared vocabulary's codes were checked
/// against [`ScalarType::max_code`] at parse; a discovered one's are drawn by
/// [`VocabularyMinter::mint`] from that same width's usable space (`vocabulary::usable_max` in
/// `tessera-store`) and so are in range by construction. Either way the narrowing here cannot
/// lose a value.
fn code_as(ty: ScalarType, code: u32) -> ScalarValue {
    match ty {
        ScalarType::U8 => ScalarValue::U8(code as u8),
        ScalarType::U16 => ScalarValue::U16(code as u16),
        // `is_category_width` admits only these three, so the fallthrough is `u32` rather than a
        // silent home for a width that should never have reached here.
        _ => ScalarValue::U32(code),
    }
}

/// The batch-level mint pre-pass for a discovered vocabulary (§3.4): collect the distinct,
/// non-null keys this Arrow batch introduces, mint each **once**, then map every row through the
/// now-complete lookup.
///
/// Not once per row: `VocabularyMinter::mint` is view-first (a bound key returns its pinned code
/// without a draw), so calling it per row would still be *correct*, but it would also be the
/// literal per-row mutation this module's callers are built to avoid, and it is what would make
/// [`BatchColumn::value`] need mutable access to a minter — which it must never have, being the
/// one place every other variant's resolution is a pure index. Collecting first and minting the
/// distinct set keeps the mutation entirely inside `decode`, before any row is read back.
///
/// An empty key is refused, never minted as [`crate::schema::ABSENT_CODE`] — the same typo trap
/// [`VocabularyMinter::mint`] itself enforces for a declared vocabulary's row-time lookup.
fn mint_batch(
    keys: &arrow::array::StringArray,
    minter: &mut VocabularyMinter,
    attribute: &crate::schema::Attribute,
) -> Result<Vec<u32>> {
    use std::collections::BTreeSet;

    let mut novel: BTreeSet<&str> = BTreeSet::new();
    for i in 0..keys.len() {
        if keys.is_null(i) {
            continue;
        }
        let key = keys.value(i);
        if minter.code_of(key).is_none() {
            novel.insert(key);
        }
    }
    for key in novel {
        minter.mint(key).map_err(|e| {
            crate::schema::schema_error(format!("attribute '{}': {e}", attribute.name))
        })?;
    }

    Ok((0..keys.len())
        .map(|i| {
            if keys.is_null(i) {
                crate::schema::ABSENT_CODE
            } else {
                minter
                    .code_of(keys.value(i))
                    .expect("every key in this batch was just minted or was already bound")
            }
        })
        .collect())
}

/// Any integer parquet column as `i64` — one conversion per batch, never per row.
fn read_integer(any: &dyn std::any::Any, ty: &DataType) -> Option<Vec<i64>> {
    use arrow::array::{
        Int16Array, Int32Array, Int64Array, Int8Array, TimestampMicrosecondArray, UInt16Array,
        UInt32Array, UInt64Array, UInt8Array,
    };
    macro_rules! widen {
        ($($dt:pat => $arr:ident),* $(,)?) => {
            match ty {
                $($dt => any
                    .downcast_ref::<$arr>()?
                    .values()
                    .iter()
                    .map(|v| *v as i64)
                    .collect(),)*
                // **Microseconds only, and the other units are refused.** Nothing records a unit:
                // `MANIFEST.declared_scalars` says `i64` (or `timestamp_us`, which fixes it). So a
                // build that silently took milliseconds from one source and microseconds from
                // another would store two incomparable numbers under one declaration, and the
                // difference would surface as dates a thousandfold wrong rather than as an error.
                // Normalising here was the alternative and is worse: it rewrites the caller's
                // values on a rule they never stated.
                DataType::Timestamp(TimeUnit::Microsecond, _) => {
                    any.downcast_ref::<TimestampMicrosecondArray>()?.values().to_vec()
                }
                _ => return None,
            }
        };
    }
    Some(widen! {
        DataType::UInt8 => UInt8Array,
        DataType::UInt16 => UInt16Array,
        DataType::UInt32 => UInt32Array,
        DataType::UInt64 => UInt64Array,
        DataType::Int8 => Int8Array,
        DataType::Int16 => Int16Array,
        DataType::Int32 => Int32Array,
        DataType::Int64 => Int64Array,
    })
}

/// A value that must fit the declared width, refused rather than truncated.
///
/// **The refusal is the point.** A `u8` category column whose data carries 300 is a build that
/// would otherwise write 44 — a different value, in a column whose width cannot be changed
/// without rewriting the corpus, with nothing downstream able to notice.
fn narrow(value: i64, min: i64, max: i64, attribute: &crate::schema::Attribute) -> Result<i64> {
    if value < min || value > max {
        return Err(crate::schema::schema_error(format!(
            "attribute '{}': the points file carries {value}, which does not fit its declared \
             '{}' ({min}..={max}). Refused rather than truncated — the width is baked into every \
             row and the remedy is a rebuild at a wider declaration (per-point-attributes §3.6)",
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
