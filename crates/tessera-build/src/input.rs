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
//! * **access terms** — either the exploded `(entity_id, term_id)` relation ([`scan_pairs`]) or a
//!   `list<string>` field of the points source itself ([`scan_access_field`]), which is where the
//!   trim and the empty rule live.
//!
//! **Every column is named by the declaration, never by this module** ([`crate::config::Fields`]).
//! A declared field the file does not carry is [`field_index`]'s refusal, and that refusal is the
//! point of the whole arrangement: an absent column reads as empty, and empty is silent in exactly
//! the directions that matter — an absent geometry column puts every point at the origin, and an
//! absent access column puts every point in no principal's mask.
//!
//! Both honour a `limit`: `entity_id < limit` selects a prefix of entity space, which is a
//! whole coherent corpus because entity IDs are append-only (I9, dataset §4.1). Row groups
//! whose statistics prove they hold no qualifying row are skipped outright — at 10⁹ items the
//! pairs relation is billions of rows and the prefix is a few hundred thousand.

use std::collections::{BTreeSet, HashMap};
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
use tessera_spatial::{fixed32, Bounds, Projection};
use tessera_store::vocabulary::VocabularyMinter;

use crate::config::{Fields, ENTITY_ID};
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
/// The exploded relation's term column (`configuration.md` §1's `point_visibility.source`).
pub const TERM_ID: &str = "term_id";

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
pub fn read_points(
    path: &Path,
    fields: &Fields,
    projection: Projection,
    extent: &Bounds,
    limit: Option<u64>,
) -> Result<Vec<PointRow>> {
    let mut out = Vec::new();
    scan_points(path, fields, projection, extent, limit, |row| {
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
    fields: &Fields,
    projection: Projection,
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
    let id_idx_in_file = field_index(path, &schema, fields, ENTITY_ID)?;
    let geometry_kind = geometry_kind(path, &schema, fields)?;
    if geometry_kind != GeometryKind::Xy && *extent != IDENTITY_EXTENT {
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
    // The column *names*, resolved: what the declaration moved, and the canonical name for
    // everything it left alone. Every index below — in this schema and in each worker's projected
    // one — is looked up by these, never by the canonical name.
    let wanted: Vec<&str> = match geometry_kind {
        GeometryKind::Xy => vec![fields.of(ENTITY_ID), fields.of("x"), fields.of("y")],
        GeometryKind::Morton => vec![fields.of(ENTITY_ID), fields.of("morton")],
        GeometryKind::MortonResidual => vec![
            fields.of(ENTITY_ID),
            fields.of("morton"),
            fields.of("residual"),
        ],
    };

    // Project: the probe corpus carries columns this build has no use for, and at 10^9 rows
    // not decoding them is the difference between one pass and two. Each decode worker builds
    // its own `ProjectionMask` from these root indices against its own reader.
    let mut roots = Vec::with_capacity(wanted.len());
    for canonical in geometry_kind.canonical_fields() {
        roots.push(field_index(path, &schema, fields, canonical)?);
    }

    let keep = prunable_row_groups(builder.metadata(), id_idx_in_file, limit);
    drop(builder);
    let workers = decode_worker_count(keep.len());
    let shards: Vec<Vec<usize>> = keep
        .chunks(keep.len().div_ceil(workers).max(1))
        .map(|c| c.to_vec())
        .collect();
    // Resolved once, on this thread, and copied into each worker: a worker re-resolves its own
    // indices against its own projected schema, and it must do so under the same names.
    let (id_name, x_name, y_name) = (fields.of(ENTITY_ID), fields.of("x"), fields.of("y"));
    let (morton_name, residual_name) = (fields.of("morton"), fields.of("residual"));

    /// One decoded batch's columns, extracted on a worker thread.
    enum PointCols {
        Xy(Vec<u64>, Vec<f64>, Vec<f64>),
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
                    let id_idx = column_index(path, &projected, id_name)?;
                    let geometry = match geometry_kind {
                        GeometryKind::Xy => Geometry::Xy(
                            column_index(path, &projected, x_name)?,
                            column_index(path, &projected, y_name)?,
                        ),
                        GeometryKind::Morton => {
                            Geometry::Morton(column_index(path, &projected, morton_name)?)
                        }
                        GeometryKind::MortonResidual => Geometry::MortonResidual(
                            column_index(path, &projected, morton_name)?,
                            column_index(path, &projected, residual_name)?,
                        ),
                    };
                    for batch in reader {
                        let batch = batch.map_err(|e| BuildError::arrow(path, e))?;
                        let ids = read_u64_column(path, &batch, id_idx, id_name)?;
                        let cols = match geometry {
                            Geometry::Xy(xi, yi) => PointCols::Xy(
                                ids,
                                read_f64_column(path, &batch, xi, x_name)?,
                                read_f64_column(path, &batch, yi, y_name)?,
                            ),
                            Geometry::Morton(mi) => PointCols::Morton(
                                ids,
                                read_u64_column(path, &batch, mi, morton_name)?,
                            ),
                            Geometry::MortonResidual(mi, ri) => PointCols::MortonResidual(
                                ids,
                                read_u64_column(path, &batch, mi, morton_name)?,
                                read_u64_column(path, &batch, ri, residual_name)?,
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
                            // The one place a coordinate is placed, reached in the width the file
                            // was read at: the column arrives as `f64` whatever width it was
                            // stored at, so nothing narrows between the Parquet page and the
                            // fixed-point grid. The transform runs here, at the boundary, in the
                            // same place a write does it (`projections.md` §3);
                            // `Projection::None` is the exact identity, so an unprojected view's
                            // stored positions are the bits they always were. A coordinate outside
                            // the projection's input domain is refused, and one outside its
                            // *output* domain clipped and counted, by the survey pass that every
                            // build runs before this one (`survey_points`).
                            let (x, y) = projection.forward(xs[i], ys[i]);
                            if visit(PointRow {
                                source_id: ids[i],
                                qx: fixed32(x, extent.x_min, extent.x_max),
                                qy: fixed32(y, extent.y_min, extent.y_max),
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
pub fn read_pairs(
    path: &Path,
    fields: &Fields,
    limit: Option<u64>,
) -> Result<HashMap<u64, Vec<u64>>> {
    let mut grouped: HashMap<u64, Vec<u64>> = HashMap::new();
    scan_pairs(path, fields, limit, |source_id, term| {
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
    fields: &Fields,
    limit: Option<u64>,
    mut visit: F,
) -> Result<()> {
    let file = File::open(path).map_err(|e| BuildError::io(path, e))?;
    let builder =
        ParquetRecordBatchReaderBuilder::try_new(file).map_err(|e| BuildError::parquet(path, e))?;
    let schema = builder.schema().clone();
    let id_idx = field_index(path, &schema, fields, ENTITY_ID)?;
    let term_idx = field_index(path, &schema, fields, TERM_ID)?;
    let (id_name, term_name) = (fields.of(ENTITY_ID), fields.of(TERM_ID));

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
                        let ids = read_u64_column(path, &batch, id_idx, id_name)?;
                        let terms = read_u64_column(path, &batch, term_idx, term_name)?;
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

// ---------------------------------------------------------------------------------------------
// Access terms read from a field of the points source
// ---------------------------------------------------------------------------------------------

/// What descriptor a **source term** names — the string a build interns into the dictionary and a
/// credential is later matched against.
///
/// Two shapes, because a view declares its access terms one of two ways (`configuration.md` §1),
/// and both reach the rest of the build as a `u64` source term so that only this type knows the
/// difference.
#[derive(Debug, Clone)]
pub enum TermDescriptors {
    /// The exploded relation's own integer `term_id`s, spelled as decimals — what the probe
    /// corpus has always carried and what `builtin:passthrough` has always been handed.
    Ids,
    /// A field-sourced view's distinct terms, **sorted**, a source term being a position in this
    /// list.
    ///
    /// **Sorted, and that is load-bearing.** Term ids are assigned by first appearance, ties
    /// broken by source term — so ordering source terms by their position here has to be ordering
    /// their descriptors, or the two builds would number the dictionary differently and place
    /// different permanent entity ids (I9).
    Vocabulary(Vec<String>),
}

impl TermDescriptors {
    /// The descriptor `source_term` names.
    pub fn descriptor(&self, source_term: u64) -> std::borrow::Cow<'_, str> {
        match self {
            TermDescriptors::Ids => std::borrow::Cow::Owned(source_term.to_string()),
            TermDescriptors::Vocabulary(terms) => {
                std::borrow::Cow::Borrowed(terms[source_term as usize].as_str())
            }
        }
    }

    /// The source term a descriptor occupies, for a vocabulary; `None` if it carries none.
    pub fn position_of(&self, descriptor: &str) -> Option<u64> {
        match self {
            TermDescriptors::Ids => descriptor.parse().ok(),
            TermDescriptors::Vocabulary(terms) => terms
                .binary_search_by(|t| t.as_str().cmp(descriptor))
                .ok()
                .map(|i| i as u64),
        }
    }
}

/// The distinct access terms a field-sourced view carries, sorted, with the default among them.
///
/// **A whole pass over one column before any term id exists**, which is the price of assigning
/// term ids by a rule both builds can compute: the linear build walks items and interns as it
/// goes, and the streaming build ranks terms by `(first ordinal, source term)` over a relation it
/// scans twice. Making the source term a position in a *sorted* list is what makes those two the
/// same ordering. The relation route pays nothing for this — its source terms are already integers
/// the file supplies.
///
/// The default is always present, because it is what a null or empty row is filled with and a fill
/// must have a term to fill with.
pub fn read_access_vocabulary(
    points: &Path,
    fields: &Fields,
    field: Option<&str>,
    default: &str,
    limit: Option<u64>,
) -> Result<Vec<String>> {
    let mut distinct: BTreeSet<String> = BTreeSet::new();
    distinct.insert(default.to_string());
    if let Some(field) = field {
        scan_access_column(points, fields, field, limit, |_, terms| {
            for term in terms {
                if !distinct.contains(*term) {
                    distinct.insert((*term).to_string());
                }
            }
            ControlFlow::Continue(())
        })?;
    }
    Ok(distinct.into_iter().collect())
}

/// The field route's counterpart to [`scan_pairs`]: one `(source_id, source_term)` per term a
/// point carries, and **one carrying the default for a point that carries none**.
///
/// Three rules, all of them decided here because this is where a row's value becomes a term:
///
/// - **A null value and an empty list both mean *no access terms*, which means visible to no
///   principal.** Neither means unrestricted. That is the reading a fill is *for*: where the view
///   declares a default, those rows get exactly it, and where it declares one that no principal
///   holds they stay invisible. The permissive misreading — *null is unspecified, so unrestricted*
///   — would put every unlabelled point in everyone's mask.
/// - **Terms are trimmed**, matching what `builtin:passthrough` already does to the label it is
///   handed, so ` cs.LG` and `cs.LG` are one term rather than two that no credential spells the
///   same way. A term that is empty after trimming is not a term.
/// - **Filling never overrides.** A point carrying terms of its own keeps exactly those. A point's
///   terms are disjunctive — `M_auth` is a union of posting lists — so a label added to a point can
///   only widen it, which makes overriding inadmissible rather than merely unwise.
///
/// `field` is `None` for a view declaring only a default, where every point takes it.
pub fn scan_access_field<F: FnMut(u64, u64) -> ControlFlow<()>>(
    points: &Path,
    fields: &Fields,
    field: Option<&str>,
    vocabulary: &[String],
    default_term: u64,
    limit: Option<u64>,
    mut visit: F,
) -> Result<AccessFill> {
    let mut fill = AccessFill::default();
    let Some(field) = field else {
        // Every point takes the default: the corpus with no permission model. Read from the
        // identity column alone, so a view declaring only a default opens no access column at all.
        scan_identity(points, fields, limit, |source_id| {
            fill.filled += 1;
            visit(source_id, default_term)
        })?;
        return Ok(fill);
    };
    // A term this pass sees and the vocabulary pass did not means the file changed underneath the
    // build. Refused rather than assumed away: the two passes must see one relation, and the
    // second is what assigns the postings.
    let mut changed: Option<BuildError> = None;
    scan_access_column(points, fields, field, limit, |source_id, terms| {
        if terms.is_empty() {
            fill.filled += 1;
            return visit(source_id, default_term);
        }
        fill.carried += 1;
        for term in terms {
            let Ok(position) = vocabulary.binary_search_by(|t| t.as_str().cmp(term)) else {
                changed = Some(BuildError::Schema {
                    path: points.to_path_buf(),
                    detail: format!(
                        "the access column '{field}' now carries the term '{term}', which it \
                         did not when this build read its vocabulary. The file changed underneath \
                         the build, and the two passes must see one relation"
                    ),
                });
                return ControlFlow::Break(());
            };
            if visit(source_id, position as u64).is_break() {
                return ControlFlow::Break(());
            }
        }
        ControlFlow::Continue(())
    })?;
    if let Some(error) = changed {
        return Err(error);
    }
    Ok(fill)
}

/// How many points carried terms of their own and how many took the view's default — reported by
/// the build, because a fill is a visibility decision and a corpus that turned out to be almost
/// entirely default is one whose author should see the number.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct AccessFill {
    pub carried: u64,
    pub filled: u64,
}

/// Walk the identity column alone, in file order.
fn scan_identity<F: FnMut(u64) -> ControlFlow<()>>(
    path: &Path,
    fields: &Fields,
    limit: Option<u64>,
    mut visit: F,
) -> Result<()> {
    let file = File::open(path).map_err(|e| BuildError::io(path, e))?;
    let builder =
        ParquetRecordBatchReaderBuilder::try_new(file).map_err(|e| BuildError::parquet(path, e))?;
    let schema = builder.schema().clone();
    let id_root = field_index(path, &schema, fields, ENTITY_ID)?;
    let keep = prunable_row_groups(builder.metadata(), id_root, limit);
    let projection = parquet::arrow::ProjectionMask::roots(builder.parquet_schema(), [id_root]);
    let reader = builder
        .with_row_groups(keep)
        .with_projection(projection)
        .with_batch_size(65_536)
        .build()
        .map_err(|e| BuildError::parquet(path, e))?;
    let projected = arrow::array::RecordBatchReader::schema(&reader);
    let id_idx = column_index(path, &projected, fields.of(ENTITY_ID))?;
    for batch in reader {
        let batch = batch.map_err(|e| BuildError::arrow(path, e))?;
        let ids = read_u64_column(path, &batch, id_idx, fields.of(ENTITY_ID))?;
        for &id in &ids {
            if limit.is_some_and(|l| id >= l) {
                continue;
            }
            if visit(id).is_break() {
                return Ok(());
            }
        }
    }
    Ok(())
}

/// Walk `(entity_id, access field)` in file order, handing each row its **trimmed, non-empty**
/// terms. Single-threaded: one string column against geometry's decode cost, and both passes over
/// it must see the same rows in the same order.
fn scan_access_column<F: FnMut(u64, &[&str]) -> ControlFlow<()>>(
    path: &Path,
    fields: &Fields,
    field: &str,
    limit: Option<u64>,
    mut visit: F,
) -> Result<()> {
    let file = File::open(path).map_err(|e| BuildError::io(path, e))?;
    let builder =
        ParquetRecordBatchReaderBuilder::try_new(file).map_err(|e| BuildError::parquet(path, e))?;
    let schema = builder.schema().clone();
    let id_root = field_index(path, &schema, fields, ENTITY_ID)?;
    // The access field is named directly by `point_visibility.field` rather than through the map:
    // it is the declaration, not a relocation of a canonical name. Absent is the same refusal a
    // moved name gets, and for the same reason — an unread access column is a corpus in no
    // principal's mask, silently.
    let access_root = schema
        .column_with_name(field)
        .map(|(i, _)| i)
        .ok_or_else(|| BuildError::Schema {
            path: path.to_path_buf(),
            detail: format!(
                "{}: `point_visibility.field = \"{field}\"` names a column this file does not \
                 carry. Its columns are: {}. Refused rather than read as empty: with no access \
                 column read, every point would carry no term and so sit in no principal\'s mask",
                fields.object(),
                column_names(&schema)
            ),
        })?;
    let keep = prunable_row_groups(builder.metadata(), id_root, limit);
    let projection =
        parquet::arrow::ProjectionMask::roots(builder.parquet_schema(), [id_root, access_root]);
    let reader = builder
        .with_row_groups(keep)
        .with_projection(projection)
        .with_batch_size(65_536)
        .build()
        .map_err(|e| BuildError::parquet(path, e))?;
    let projected = arrow::array::RecordBatchReader::schema(&reader);
    let id_idx = column_index(path, &projected, fields.of(ENTITY_ID))?;
    let access_idx = column_index(path, &projected, field)?;

    for batch in reader {
        let batch = batch.map_err(|e| BuildError::arrow(path, e))?;
        let ids = read_u64_column(path, &batch, id_idx, fields.of(ENTITY_ID))?;
        let terms = read_access_column(path, batch.column(access_idx), field)?;
        let mut row: Vec<&str> = Vec::new();
        for (i, &id) in ids.iter().enumerate() {
            if limit.is_some_and(|l| id >= l) {
                continue;
            }
            row.clear();
            row.extend(terms.terms_of(i));
            if visit(id, &row).is_break() {
                return Ok(());
            }
        }
    }
    Ok(())
}

/// One batch's access column, flattened: row `i`'s terms are `terms[bounds[i]..bounds[i + 1]]`.
struct AccessBatch {
    bounds: Vec<usize>,
    terms: Vec<String>,
}

impl AccessBatch {
    fn terms_of(&self, row: usize) -> impl Iterator<Item = &str> {
        self.terms[self.bounds[row]..self.bounds[row + 1]]
            .iter()
            .map(String::as_str)
    }
}

/// Decode one batch of the access column, applying the trim and the empty rule.
///
/// **A `list<string>`, or a plain `string` where a point carries one term** (`configuration.md`
/// §1). Any other type is refused rather than coerced: a column of integers or of a nested struct
/// is not a term list, and guessing what its rows meant would mint access terms nobody wrote.
fn read_access_column(path: &Path, column: &arrow::array::ArrayRef, name: &str) -> Result<AccessBatch> {
    use arrow::array::{Array as _, LargeStringArray, ListArray, StringArray};

    let rows = column.len();
    let mut batch = AccessBatch {
        bounds: Vec::with_capacity(rows + 1),
        terms: Vec::new(),
    };
    batch.bounds.push(0);
    fn push(batch: &mut AccessBatch, value: &str) {
        let term = value.trim();
        if term.is_empty() {
            return;
        }
        batch.terms.push(term.to_string());
    }

    if let Some(values) = column.as_any().downcast_ref::<StringArray>() {
        for i in 0..rows {
            if !values.is_null(i) {
                push(&mut batch, values.value(i));
            }
            batch.bounds.push(batch.terms.len());
        }
        return Ok(batch);
    }
    if let Some(values) = column.as_any().downcast_ref::<LargeStringArray>() {
        for i in 0..rows {
            if !values.is_null(i) {
                push(&mut batch, values.value(i));
            }
            batch.bounds.push(batch.terms.len());
        }
        return Ok(batch);
    }
    if let Some(list) = column.as_any().downcast_ref::<ListArray>() {
        let values = list.values();
        let strings = values
            .as_any()
            .downcast_ref::<StringArray>()
            .ok_or_else(|| BuildError::Schema {
                path: path.to_path_buf(),
                detail: format!(
                    "the access column '{name}' is a list of {:?}, and an access term is a \
                     string",
                    values.data_type()
                ),
            })?;
        let offsets = list.value_offsets();
        for i in 0..rows {
            if !list.is_null(i) {
                for j in offsets[i]..offsets[i + 1] {
                    let j = j as usize;
                    if !strings.is_null(j) {
                        push(&mut batch, strings.value(j));
                    }
                }
            }
            batch.bounds.push(batch.terms.len());
        }
        return Ok(batch);
    }
    Err(BuildError::Schema {
        path: path.to_path_buf(),
        detail: format!(
            "the access column '{name}' has type {:?}. A point\'s access terms are a \
             `list<string>`, or a plain `string` where a point carries one term \
             (configuration.md §1). Refused rather than coerced: guessing what another type\'s \
             rows meant would mint access terms nobody wrote",
            column.data_type()
        ),
    })
}

/// What one pass over a view's geometry establishes: the box the data actually occupies, and how
/// many of its rows a stated extent would **clamp** onto the frame's boundary.
///
/// **Both halves come out of one pass**, which is what makes reporting the clamp affordable at
/// every build rather than only under `auto`. `auto` needs the box; a stated extent needs the
/// clamp count; neither needs the other's pass.
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum PointSurvey {
    /// The source carries coordinates, so a frame decides where every point lands.
    Coordinates(CoordinateSurvey),
    /// The source carries Morton codes: the position is already quantised, in the grid's own
    /// frame, and [`scan_points`] reassembles it rather than quantising it. Nothing clamps, and
    /// there is no box to fit — which is why `auto` over such a file is refused instead.
    Quantised,
}

/// The box the data occupies, and what a frame does to it.
///
/// **A clamp is `v < min` or `v > max`, and `v == max` is not one.** Cells are half-open and the
/// maximum lands in the top cell by construction (`tessera_spatial::morton`), so counting the
/// boundary value as a clamp would report every tightly-fitted corpus as damaged.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct CoordinateSurvey {
    /// Rows the build would place — `limit` honoured.
    pub rows: u64,
    /// The tightest box holding every one of them. `None` when the selection is empty.
    pub bounds: Option<Bounds>,
    /// Rows clamped on **either** axis: their stored position is the frame's boundary rather than
    /// their own. Always `0` where no frame was supplied.
    pub clamped: u64,
    /// Rows clamped on x, and on y. A row outside on both axes counts in both, and once in
    /// [`CoordinateSurvey::clamped`].
    pub clamped_x: u64,
    pub clamped_y: u64,
    /// Rows whose latitude fell outside the **projection's** own domain — for `web_mercator`,
    /// beyond ±85.0511287798066° (`projections.md` §7). Always `0` under `projection = "none"`,
    /// which has no domain, and never a clamp: clipping lands a point exactly on the frame's
    /// edge, which is where the clamp rule says a point is *not* clamped, so the two counters
    /// cannot see each other's rows and a shared one would report the wrong cause.
    pub clipped: u64,
}

impl CoordinateSurvey {
    /// The share of surveyed rows whose stored position is the frame's boundary rather than their
    /// own. `0.0` for an empty selection, there being no row to misplace.
    pub fn clamped_fraction(&self) -> f64 {
        if self.rows == 0 {
            0.0
        } else {
            self.clamped as f64 / self.rows as f64
        }
    }
}

/// The tightest box holding every point this build would read, and — where `against` supplies a
/// frame — how many of those rows that frame clamps.
///
/// **A full pass over two columns, not the file's statistics.** Parquet min/max are per row group
/// and may be absent, so a statistics route would make the extent — and therefore every stored
/// cell — depend on how the producer happened to lay the file out, and would silently widen the
/// box for a file that carries none. `auto` is already the spelling that says *fit the data I have*
/// (`configuration.md` §1); making it also mean *approximately, depending on the writer* is the
/// kind of quiet dependence a rebuild discovers as moved geometry. It also could not answer the
/// clamp question at all: a row group's bounds say nothing about how many of its rows sit outside
/// the frame.
///
/// `limit` is honoured, because the extent must frame the rows the build actually places: a
/// prefix build whose box was computed over the whole file would quantise its rows into a
/// fraction of the grid.
///
/// A Morton points file has no coordinates to bound. With a frame in hand that is simply
/// [`PointSurvey::Quantised`] — nothing is quantised at build, so nothing clamps. With none it is
/// refused rather than defaulted to the grid's own extent: the two Morton branches are exact only
/// against [`IDENTITY_EXTENT`], so the answer is a line in the config rather than a guess in the
/// reader.
pub fn survey_points(
    path: &Path,
    fields: &Fields,
    projection: Projection,
    limit: Option<u64>,
    against: Option<&Bounds>,
) -> Result<PointSurvey> {
    let file = File::open(path).map_err(|e| BuildError::io(path, e))?;
    let builder =
        ParquetRecordBatchReaderBuilder::try_new(file).map_err(|e| BuildError::parquet(path, e))?;
    let schema = builder.schema().clone();
    let (x_name, y_name) = (fields.of("x"), fields.of("y"));
    if schema.column_with_name(x_name).is_none() || schema.column_with_name(y_name).is_none() {
        let coded = schema.column_with_name(fields.of("morton")).is_some();
        // **A projected view has no Morton geometry, and the refusal belongs here** — the same
        // rule `compile_projected_fields` applies to `fields.morton`, reaching the file that
        // carries the column rather than the declaration that names it. Without this the survey
        // answers `Quantised`, the build prints that the points arrive already placed and nothing
        // is quantised here, and the scan a moment later refuses for a missing `lon` — loud, but
        // from the wrong place and having first legitimised a shape this design has none of.
        if coded && projection != Projection::None {
            return Err(BuildError::Schema {
                path: path.to_path_buf(),
                detail: format!(
                    "{}: this points file stores Morton codes, and this view is projected ({}). \
                     A code is a position already placed in a frame, so there is no longitude \
                     left for a projection to transform (projections.md §3). Either declare \
                     `projection = \"none\"` and read the codes against the grid's own frame, or \
                     supply '{x_name}'/'{y_name}' columns",
                    fields.object(),
                    projection.name()
                ),
            });
        }
        if against.is_some() && coded {
            return Ok(PointSurvey::Quantised);
        }
        return Err(BuildError::Schema {
            path: path.to_path_buf(),
            detail: if coded {
                "`extent = \"auto\"` fits a box around this view\'s coordinates, and this points \
                 file stores Morton codes rather than coordinates. Codes are exact only against \
                 the grid\'s own extent, so write it out: `extent = { min = 0.0, max = 65536.0 }`."
                    .to_string()
            } else {
                format!(
                    "{}: `extent = \"auto\"` fits a box around this view\'s coordinates, and this \
                     file carries no \'{x_name}\'/\'{y_name}\' pair to fit one around. Its columns \
                     are: {}",
                    fields.object(),
                    column_names(&schema)
                )
            },
        });
    }

    let id_idx_in_file = field_index(path, &schema, fields, ENTITY_ID)?;
    let keep = prunable_row_groups(builder.metadata(), id_idx_in_file, limit);
    let mut roots = Vec::with_capacity(3);
    for canonical in [ENTITY_ID, "x", "y"] {
        roots.push(field_index(path, &schema, fields, canonical)?);
    }
    let mask = parquet::arrow::ProjectionMask::roots(builder.parquet_schema(), roots);
    let reader = builder
        .with_row_groups(keep)
        .with_projection(mask)
        .with_batch_size(65_536)
        .build()
        .map_err(|e| BuildError::parquet(path, e))?;
    let projected = arrow::array::RecordBatchReader::schema(&reader);
    let id_idx = column_index(path, &projected, fields.of(ENTITY_ID))?;
    let x_idx = column_index(path, &projected, x_name)?;
    let y_idx = column_index(path, &projected, y_name)?;

    let mut found = false;
    let (mut x_min, mut x_max) = (f64::INFINITY, f64::NEG_INFINITY);
    let (mut y_min, mut y_max) = (f64::INFINITY, f64::NEG_INFINITY);
    let mut survey = CoordinateSurvey {
        rows: 0,
        bounds: None,
        clamped: 0,
        clamped_x: 0,
        clamped_y: 0,
        clipped: 0,
    };
    for batch in reader {
        let batch = batch.map_err(|e| BuildError::arrow(path, e))?;
        let ids = read_u64_column(path, &batch, id_idx, fields.of(ENTITY_ID))?;
        let xs = read_f64_column(path, &batch, x_idx, x_name)?;
        let ys = read_f64_column(path, &batch, y_idx, y_name)?;
        for i in 0..ids.len() {
            if limit.is_some_and(|l| ids[i] >= l) {
                continue;
            }
            // A non-finite coordinate would poison every comparison below and produce a box the
            // extent validator then refuses with no mention of the row that caused it. Named
            // here, where the file and the value are both in hand.
            let (x, y) = (xs[i], ys[i]);
            if !x.is_finite() || !y.is_finite() {
                return Err(BuildError::Schema {
                    path: path.to_path_buf(),
                    detail: format!(
                        "{} {} has a non-finite position ({x}, {y}), so no box fits the \
                         data. `extent = \"auto\"` reads every row it would place",
                        fields.of(ENTITY_ID),
                        ids[i]
                    ),
                });
            }
            // **The transform runs here, and the two things it can find are different.** A
            // coordinate outside WGS84's own range is not a coordinate and is refused
            // (`projections.md` §2); a latitude inside that range but outside the *projection's*
            // domain is clipped onto the frame's edge, counted, and never refused (§7) — the
            // clamp counter below structurally cannot see one, because the edge is exactly where
            // it says nothing is clamped. Every build takes this pass before any work, so it is
            // the one place the check has to be.
            let (x, y) = if projection == Projection::None {
                (x, y)
            } else {
                if x.abs() > 180.0 || y.abs() > 90.0 {
                    return Err(BuildError::Schema {
                        path: path.to_path_buf(),
                        detail: format!(
                            "{} {} is at lon {x}, lat {y}, which is not a place: this view is \
                             projected ({}), and the accepted input coordinate system is WGS84 \
                             degrees — longitude within ±180, latitude within ±90 \
                             (projections.md §2). Convert the source to WGS84 before building, or \
                             declare `projection = \"none\"` if this view's space is not the Earth",
                            fields.of(ENTITY_ID),
                            ids[i],
                            projection.name()
                        ),
                    });
                }
                survey.clipped += u64::from(projection.is_clipped(y));
                projection.forward(x, y)
            };
            found = true;
            survey.rows += 1;
            x_min = x_min.min(x);
            x_max = x_max.max(x);
            y_min = y_min.min(y);
            y_max = y_max.max(y);
            if let Some(frame) = against {
                // `v == max` is **not** a clamp: cells are half-open and the maximum lands in the
                // top cell by construction, so a tightly-fitted corpus must not report its own
                // boundary rows as misplaced.
                let out_x = x < frame.x_min || x > frame.x_max;
                let out_y = y < frame.y_min || y > frame.y_max;
                survey.clamped_x += u64::from(out_x);
                survey.clamped_y += u64::from(out_y);
                survey.clamped += u64::from(out_x || out_y);
            }
        }
    }
    survey.bounds = found.then_some(Bounds {
        x_min,
        x_max,
        y_min,
        y_max,
    });
    Ok(PointSurvey::Coordinates(survey))
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
#[derive(Clone, Copy, PartialEq, Eq)]
enum GeometryKind {
    Xy,
    Morton,
    MortonResidual,
}

impl GeometryKind {
    /// The canonical fields this shape reads, identity first — the list `fields` is resolved
    /// against to build the projection.
    fn canonical_fields(self) -> &'static [&'static str] {
        match self {
            GeometryKind::Xy => &[ENTITY_ID, "x", "y"],
            GeometryKind::Morton => &[ENTITY_ID, "morton"],
            GeometryKind::MortonResidual => &[ENTITY_ID, "morton", "residual"],
        }
    }
}

/// Which geometry shape a points file offers, **the declaration deciding before the file does**.
///
/// A `fields` map naming `x` or `y` is the caller saying *this file holds coordinates*, so a miss
/// on that name is [`field_index`]'s refusal rather than a quiet fall through to a Morton column
/// that happens to be there under its canonical name — which would quantise a corpus against the
/// wrong frame and produce a well-formed bundle with the geometry wrong. With no map, presence
/// decides, as it always has. The two shapes being mutually exclusive is checked at parse
/// (`configuration.md` §1).
fn geometry_kind(
    path: &Path,
    schema: &arrow::datatypes::Schema,
    fields: &Fields,
) -> Result<GeometryKind> {
    let has = |canonical: &str| schema.column_with_name(fields.of(canonical)).is_some();
    if fields.names("x") || fields.names("y") {
        return Ok(GeometryKind::Xy);
    }
    if fields.names("morton") || fields.names("residual") {
        return Ok(if has("residual") {
            GeometryKind::MortonResidual
        } else {
            GeometryKind::Morton
        });
    }
    if has("x") && has("y") {
        return Ok(GeometryKind::Xy);
    }
    if has("morton") {
        return Ok(if has("residual") {
            GeometryKind::MortonResidual
        } else {
            GeometryKind::Morton
        });
    }
    Err(BuildError::Schema {
        path: path.to_path_buf(),
        detail: format!(
            "{}: this points file carries neither an '{}'/'{}' pair nor a '{}' column, so it \
             holds no geometry to place. Its columns are: {}",
            fields.object(),
            fields.of("x"),
            fields.of("y"),
            fields.of("morton"),
            column_names(schema)
        ),
    })
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

/// The column index of `canonical` under the names the declaration resolved — or a refusal naming
/// the object, the field, the column it looked for, and the columns the file actually carries.
///
/// **The refusal a `fields` map needs and a parser cannot make.** The map is checked against the
/// declaration at parse — every name known, every name declared — but whether the *file* has a
/// column of that name needs the file open, which is here. Without this the miss would read as an
/// absent column, and absent is silent in both directions that matter: an absent geometry column
/// puts every point at the origin, and an absent access column puts every point in no principal's
/// mask. Either is a blank map with no error anywhere, which is precisely the shape a `fields` map
/// exists to make impossible.
fn field_index(
    path: &Path,
    schema: &arrow::datatypes::Schema,
    fields: &Fields,
    canonical: &str,
) -> Result<usize> {
    let name = fields.of(canonical);
    schema
        .column_with_name(name)
        .map(|(i, _)| i)
        .ok_or_else(|| BuildError::Schema {
            path: path.to_path_buf(),
            detail: format!(
                "{}: field `{canonical}` is read from a column named '{name}', which this file \
                 does not carry. Its columns are: {}. A declared field the file lacks is refused \
                 rather than read as an empty column, an empty column being silent in exactly the \
                 directions that matter",
                fields.object(),
                column_names(schema)
            ),
        })
}

/// Every column the file carries, for a refusal to spell out.
fn column_names(schema: &arrow::datatypes::Schema) -> String {
    let names: Vec<&str> = schema.fields().iter().map(|f| f.name().as_str()).collect();
    if names.is_empty() {
        "none".to_string()
    } else {
        names.join(", ")
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

/// Read a coordinate column as `f64`, **accepting both float widths and widening the narrower**.
///
/// This is the rule an attribute column declared `f64` is already read by — `f64` accepts `f32`
/// and widens — and a coordinate takes only that half of it. The other half, `f32` accepting `f64`
/// and rounding, has no counterpart here: an attribute's width is *declared*, so narrowing is what
/// the declaration asked for, whereas a coordinate's width is a property of the corpus and nothing
/// asks for it to be reduced.
///
/// **Widening rather than narrowing is what makes a deep frame honest.** A frame at zoom offset
/// *k* resolves `2^(8−k)` `f32` steps per cell, so past roughly offset 8 a narrowing decides which
/// **cell** a point occupies rather than merely its position within one — and no report downstream
/// can see that it did, the quantiser having been handed a value the file did not hold
/// (`projections.md` §6). A whole-world frame is served perfectly well by `f32` input, which is
/// why the narrower width is accepted rather than refused.
fn read_f64_column(path: &Path, batch: &RecordBatch, idx: usize, name: &str) -> Result<Vec<f64>> {
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
            .iter()
            .map(|v| f64::from(*v))
            .collect()),
        DataType::Float64 => Ok(column
            .as_any()
            .downcast_ref::<Float64Array>()
            .expect("checked data type")
            .values()
            .to_vec()),
        other => Err(BuildError::Schema {
            path: path.to_path_buf(),
            detail: format!("column '{name}' has unsupported type {other:?}"),
        }),
    }
}

/// Read a vocabulary file: `key`, an **optional** `code`, and an optional `title`.
///
/// **Parquet, like every other build input**, so a 400-value published vocabulary is the same
/// kind of artifact as the points and pairs files and needs no second reader.
///
/// **`code` may be absent, and its absence assigns rather than defaults** (`configuration.md` §1):
/// a sourced value set is the same pair of choices an inline one is — where the values come from,
/// and whether the codes are pinned. A caller who does not care which integer a value gets should
/// not have to invent one. Absent for *some* rows and present for others is refused: which half
/// the file meant would be decided by row order.
///
/// A `gate` column is **refused rather than ignored** (⊘, `per-point-attributes.md` §3.8): an
/// explicit gate label replaces membership-derivation for its value, which is an authorisation
/// statement, and a build that silently dropped it would produce a bundle whose vocabulary is more
/// visible than its author declared. There is no vocabulary-visibility evaluation yet to honour
/// it, so refusing is the only answer that does not manufacture an assurance.
pub fn read_vocabulary_file(
    path: &Path,
    vocabulary: &str,
    fields: &Fields,
) -> Result<crate::config::DeclaredValues> {
    use arrow::array::StringArray;

    let file = File::open(path).map_err(|e| BuildError::io(path, e))?;
    let builder =
        ParquetRecordBatchReaderBuilder::try_new(file).map_err(|e| BuildError::parquet(path, e))?;
    let schema = builder.schema().clone();
    if schema.column_with_name("gate").is_some() {
        return Err(crate::config::declaration_error(format!(
            "vocabulary '{vocabulary}': the file at {} carries a `gate` column, which is \
             specified and not built (per-point-attributes §3.8). An explicit gate label replaces \
             membership-derivation for its value — an authorisation statement — and nothing \
             evaluates one yet. Refused rather than dropped: a dropped gate is a value more \
             visible than its author declared",
            path.display()
        )));
    }
    // `key` is the one field a value file must carry; `code` and `title` are absent-or-present by
    // design (absence assigns codes, and a value may have no title). A *declared* `code` or
    // `title` the file lacks is still a refusal — the map says where a field is, and a name it
    // gives that nothing carries is a column its author believes is being read.
    let key_idx = field_index(path, &schema, fields, "key")?;
    let code_idx = match fields.names("code") {
        true => Some(field_index(path, &schema, fields, "code")?),
        false => schema.column_with_name("code").map(|(i, _)| i),
    };
    let title_idx = match fields.names("title") {
        true => Some(field_index(path, &schema, fields, "title")?),
        false => schema.column_with_name("title").map(|(i, _)| i),
    };

    let reader = builder.build().map_err(|e| BuildError::parquet(path, e))?;

    let mut set = crate::config::DeclaredValues::default();
    for batch in reader {
        let batch = batch.map_err(|e| BuildError::arrow(path, e))?;
        let keys = batch
            .column(key_idx)
            .as_any()
            .downcast_ref::<StringArray>()
            .ok_or_else(|| BuildError::Schema {
                path: path.to_path_buf(),
                detail: format!("vocabulary column '{}' must be utf8", fields.of("key")),
            })?;
        let code_values = match code_idx {
            Some(idx) => Some(read_u64_column(path, &batch, idx, fields.of("code"))?),
            None => None,
        };
        let title_values = match title_idx {
            Some(idx) => Some(
                batch
                    .column(idx)
                    .as_any()
                    .downcast_ref::<StringArray>()
                    .ok_or_else(|| BuildError::Schema {
                        path: path.to_path_buf(),
                        detail: format!("vocabulary column '{}' must be utf8", fields.of("title")),
                    })?
                    .clone(),
            ),
            None => None,
        };
        for row in 0..batch.num_rows() {
            let key = keys.value(row).to_string();
            // A duplicate key here is a duplicate *code assignment*, which the caller's file
            // decides silently by row order unless it is refused. `check_codes` catches two keys
            // at one code; this catches one key at two.
            if set.order.iter().any(|seen| seen == &key) {
                return Err(crate::config::declaration_error(format!(
                    "vocabulary '{vocabulary}': the file at {} lists key '{key}' twice. Which \
                     code every row carrying '{key}' would mean is decided by row order, so it is \
                     refused",
                    path.display()
                )));
            }
            set.order.push(key.clone());
            if let Some(codes) = &code_values {
                let raw = codes[row];
                let code = u32::try_from(raw).map_err(|_| {
                    crate::config::declaration_error(format!(
                        "vocabulary '{vocabulary}': value '{key}' has code {raw}, which is not a \
                         u32"
                    ))
                })?;
                set.codes.insert(key.clone(), code);
            }
            if let Some(values) = &title_values {
                if !values.is_null(row) {
                    set.titles.insert(key, values.value(row).to_string());
                }
            }
        }
    }
    // `reserved` has no file spelling: a tombstone belongs in the reviewed config rather than in a
    // regenerable data file, on §3.4's argument that a re-sorted or regenerated vocabulary file
    // must not be able to change what a stored code means.
    Ok(set)
}

/// Stream one attribute source's columns, calling `visit(entity_id, values)` once per selected row
/// with `columns`' values in the order `columns` gives them — a subsequence of declared order,
/// which is the order `columns.arrow`'s tail is written and read back in.
///
/// **`columns` is one source's group, not the whole schema** (`configuration.md` §1's
/// `[sources]`): each attribute names the file it is read from, so a declaration whose columns sit
/// in three files calls this three times, each over the columns that named that file. `schema_decl`
/// is still the whole schema, because a vocabulary is shared across sources and a category's key is
/// resolved against the declaration rather than against the file it arrived in.
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
/// data. A row whose category column is null carries [`crate::config::ABSENT_CODE`], for either
/// kind.
///
/// `minters` is threaded through rather than owned here so the caller can hand its final state —
/// every binding this scan minted, on top of whatever the schema seeded it with — to the manifest
/// writer once the whole scan (there is exactly one, per build) has completed.
pub fn scan_attributes<F: FnMut(u64, &mut Vec<ScalarValue>)>(
    path: &Path,
    fields: &Fields,
    schema_decl: &crate::config::Schema,
    columns: &[&crate::config::Attribute],
    minters: &mut HashMap<String, VocabularyMinter>,
    limit: Option<u64>,
    mut visit: F,
) -> Result<()> {
    if columns.is_empty() {
        return Ok(());
    }
    let file = File::open(path).map_err(|e| BuildError::io(path, e))?;
    let builder =
        ParquetRecordBatchReaderBuilder::try_new(file).map_err(|e| BuildError::parquet(path, e))?;
    let file_schema = builder.schema().clone();

    let mut roots = vec![field_index(path, &file_schema, fields, ENTITY_ID)?];
    for attribute in columns {
        roots.push(
            file_schema
                .column_with_name(attribute.column())
                .map(|(i, _)| i)
                .ok_or_else(|| BuildError::Schema {
                    path: path.to_path_buf(),
                    detail: format!(
                        "the schema declares attribute '{}', read from a column named '{}', which \
                         this attribute source has no column for. Its columns are: {}. A declared \
                         column the data lacks would otherwise be written as the absent sentinel \
                         for every row — a column that cost its width to say nothing",
                        attribute.name,
                        attribute.column(),
                        column_names(&file_schema)
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
    let id_idx = column_index(path, &projected, fields.of(ENTITY_ID))?;
    let attribute_idx: Vec<usize> = columns
        .iter()
        .map(|a| column_index(path, &projected, a.column()))
        .collect::<Result<_>>()?;

    let mut row_values: Vec<ScalarValue> = Vec::with_capacity(columns.len());
    for batch in reader {
        let batch = batch.map_err(|e| BuildError::arrow(path, e))?;
        let ids = read_u64_column(path, &batch, id_idx, fields.of(ENTITY_ID))?;

        // **Decoded once per batch, not once per row.** An earlier revision called a
        // whole-column converter from inside the row loop, so a 65,536-row batch decoded its
        // integer columns 65,536 times — quadratic in the batch size, and invisible at the scale
        // a test uses. A discovered category's mint pre-pass rides the same discipline: minting
        // is per distinct key in the batch, decided here, not per row.
        let mut decoded: Vec<BatchColumn> = Vec::with_capacity(columns.len());
        for (attribute, &idx) in columns.iter().zip(&attribute_idx) {
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
            // **Handed over rather than lent**: the visitor moves each value into its staging
            // column, where borrowing it made every string in the source a second copy for the
            // one line that landed it. The clear here is what makes that safe — a visitor that
            // drains leaves nothing, one that returns early leaves a row this overwrites.
            row_values.clear();
            for (attribute, column) in columns.iter().zip(&decoded) {
                row_values.push(column.value(row, attribute, schema_decl)?);
            }
            visit(entity_id, &mut row_values);
        }
    }
    Ok(())
}

/// One batch's worth of a declared column, decoded to the shape the row loop indexes, together
/// with the source's own record of which rows carry nothing.
///
/// **The null buffer is kept rather than dropped, and that is the whole of decision 0064's build
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
    /// A per-item string, for an `index`-only `utf8` column.
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
        attribute: &crate::config::Attribute,
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
        attribute: &crate::config::Attribute,
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
            return match attribute.value_set {
                Some(crate::config::ValueSet::Open) => {
                    let minter = minters.get_mut(vocabulary).unwrap_or_else(|| {
                        panic!(
                            "'{vocabulary}' is discovered, so `Schema::open_minters` must \
                             have seeded it before this scan began"
                        )
                    });
                    Ok(BatchValues::Discovered(mint_batch(
                        keys, minter, attribute,
                    )?))
                }
                // Declared (or a vocabulary shared by naming it): resolved per row in `value`,
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
            // Reached only by an `index`-only column: `render` on either string type is still
            // refused at parse (§4.3 — the hot column is a fixed-width slot per row), but an
            // indexed column lives in entity space and costs the hot column nothing.
            //
            // **A keyword reads exactly as a `utf8` does, and that is the family's whole input
            // contract.** The dictionary and the ordinal are storage, minted where the layer is
            // written; a points file carries the values themselves, so there is nothing here to
            // resolve and no ordinal to be wrong about (records §7, "ingest wire").
            //
            // **Text reads the same way, for the same reason and one more.** Its terms are minted
            // by an analyser at index time and its prose goes to the record blob, so what a points
            // file carries is the prose and nothing else — there is no term column to supply and
            // no analyser to run this side of the declaration (records §4.4).
            ScalarType::Utf8 | ScalarType::Keyword | ScalarType::Text => BatchValues::Text(
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

    /// Whether a source column of type `found` can carry an attribute declared as `attribute` —
    /// **the schema-only half of [`BatchColumn::decode_values`]**, which is what `tessera check`
    /// can answer without reading a row.
    ///
    /// It restates the downcasts above rather than sharing them, because a schema has no array to
    /// downcast; the two are held together by `column_carries_agrees_with_the_decoder`, which
    /// walks every declared type against every Arrow type this build can meet and asserts the two
    /// give one answer. Without that test this function is a second opinion, and a check that says
    /// *fine* where the build says *mismatch* is worse than no check at all.
    fn carries(attribute: &crate::config::Attribute, found: &DataType) -> bool {
        if attribute.vocabulary.is_some() {
            // A category arrives as its *key*, never as a code.
            return matches!(found, DataType::Utf8);
        }
        match attribute.ty {
            ScalarType::Bool => matches!(found, DataType::Boolean),
            // `f32` accepts `f64` and rounds; `f64` accepts `f32` and widens.
            ScalarType::F32 | ScalarType::F64 => {
                matches!(found, DataType::Float32 | DataType::Float64)
            }
            ScalarType::Utf8 | ScalarType::Keyword | ScalarType::Text => {
                matches!(found, DataType::Utf8)
            }
            // Every integer family widens to `i64` and is range-checked per row, which a schema
            // cannot anticipate — so this is presence and family, never fit.
            _ => matches!(
                found,
                DataType::UInt8
                    | DataType::UInt16
                    | DataType::UInt32
                    | DataType::UInt64
                    | DataType::Int8
                    | DataType::Int16
                    | DataType::Int32
                    | DataType::Int64
                    | DataType::Timestamp(TimeUnit::Microsecond, _)
            ),
        }
    }

    fn value(
        &self,
        row: usize,
        attribute: &crate::config::Attribute,
        schema_decl: &crate::config::Schema,
    ) -> Result<ScalarValue> {
        // **Absence, for every family that has no in-band marker.** The two that do are handled in
        // their own arms below and never reach this: a category spends the reserved code 0, and
        // `Text` carries the source's null through itself. Everything else is a number, whose every
        // bit pattern is a legal value — so the source's null buffer is the only thing that
        // distinguishes "carries no score" from "scores zero", and reading `values()` past it
        // silently makes the two the same (decision 0064).
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
                    crate::config::ABSENT_CODE
                } else {
                    let key = keys.value(row);
                    let vocabulary = attribute
                        .vocabulary
                        .as_ref()
                        .expect("a Keys column belongs to a category");
                    schema_decl.vocabularies[vocabulary]
                        .code_of(key)
                        .ok_or_else(|| {
                            crate::config::declaration_error(format!(
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
/// An empty key is refused, never minted as [`crate::config::ABSENT_CODE`] — the same typo trap
/// [`VocabularyMinter::mint`] itself enforces for a declared vocabulary's row-time lookup.
fn mint_batch(
    keys: &arrow::array::StringArray,
    minter: &mut VocabularyMinter,
    attribute: &crate::config::Attribute,
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
            crate::config::declaration_error(format!("attribute '{}': {e}", attribute.name))
        })?;
    }

    Ok((0..keys.len())
        .map(|i| {
            if keys.is_null(i) {
                crate::config::ABSENT_CODE
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

/// Whether an attribute source's column of type `found` can carry `attribute` — see
/// [`BatchColumn::carries`], whose rule this is.
pub fn column_carries(attribute: &crate::config::Attribute, found: &DataType) -> bool {
    BatchColumn::carries(attribute, found)
}

/// A value that must fit the declared width, refused rather than truncated.
///
/// **The refusal is the point.** A `u8` category column whose data carries 300 is a build that
/// would otherwise write 44 — a different value, in a column whose width cannot be changed
/// without rewriting the corpus, with nothing downstream able to notice.
fn narrow(value: i64, min: i64, max: i64, attribute: &crate::config::Attribute) -> Result<i64> {
    if value < min || value > max {
        return Err(crate::config::declaration_error(format!(
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

    /// **The schema-only type check and the decoder must give one answer**, for every declared
    /// type against every Arrow type this build can meet. A `tessera check` that passes a column
    /// the build then refuses is a wasted CI run; one that refuses a column the build accepts is
    /// worse — it makes the check something a caller learns to ignore.
    #[test]
    fn column_carries_agrees_with_the_decoder() {
        use arrow::array::{
            ArrayRef, BooleanArray, Float32Array, Float64Array, Int16Array, Int32Array, Int64Array,
            Int8Array, StringArray, TimestampMicrosecondArray, UInt16Array, UInt32Array,
            UInt64Array, UInt8Array,
        };
        use arrow::datatypes::TimeUnit;
        use std::sync::Arc;

        fn attribute(ty: ScalarType, vocabulary: Option<&str>) -> crate::config::Attribute {
            crate::config::Attribute {
                name: "a".to_string(),
                title: None,
                field: None,
                ty,
                analyser: None,
                vocabulary: vocabulary.map(str::to_string),
                value_set: vocabulary.map(|_| crate::config::ValueSet::Closed),
                index: false,
                render: false,
            }
        }

        let columns: Vec<ArrayRef> = vec![
            Arc::new(BooleanArray::from(vec![true])),
            Arc::new(UInt8Array::from(vec![1u8])),
            Arc::new(UInt16Array::from(vec![1u16])),
            Arc::new(UInt32Array::from(vec![1u32])),
            Arc::new(UInt64Array::from(vec![1u64])),
            Arc::new(Int8Array::from(vec![1i8])),
            Arc::new(Int16Array::from(vec![1i16])),
            Arc::new(Int32Array::from(vec![1i32])),
            Arc::new(Int64Array::from(vec![1i64])),
            Arc::new(Float32Array::from(vec![1.0f32])),
            Arc::new(Float64Array::from(vec![1.0f64])),
            Arc::new(StringArray::from(vec!["k"])),
            Arc::new(TimestampMicrosecondArray::from(vec![1i64])),
            // The near miss the decoder names outright: a timestamp in the wrong unit.
            Arc::new(
                arrow::array::TimestampMillisecondArray::from(vec![1i64]),
            ),
        ];
        let declared = [
            ScalarType::Bool,
            ScalarType::U8,
            ScalarType::U16,
            ScalarType::U32,
            ScalarType::U64,
            ScalarType::I8,
            ScalarType::I16,
            ScalarType::I32,
            ScalarType::I64,
            ScalarType::F32,
            ScalarType::F64,
            ScalarType::TimestampUs,
            ScalarType::Utf8,
            ScalarType::Keyword,
            ScalarType::Text,
        ];
        let path = Path::new("in-memory");
        for ty in declared {
            for vocabulary in [None, Some("v")] {
                // A category's width is the vocabulary's, so only the integer widths pair with one.
                if vocabulary.is_some() && !matches!(ty, ScalarType::U8 | ScalarType::U16 | ScalarType::U32) {
                    continue;
                }
                let attribute = attribute(ty, vocabulary);
                for column in &columns {
                    let mut minters = HashMap::new();
                    let decoded =
                        BatchColumn::decode_values(path, column, &attribute, &mut minters).is_ok();
                    assert_eq!(
                        column_carries(&attribute, column.data_type()),
                        decoded,
                        "declared {ty:?} (vocabulary {vocabulary:?}) against {:?}",
                        column.data_type()
                    );
                }
            }
        }
        // And the unit that must not pass, stated outright rather than left to the loop.
        assert!(!column_carries(
            &attribute(ScalarType::TimestampUs, None),
            &DataType::Timestamp(TimeUnit::Millisecond, None)
        ));
    }

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
