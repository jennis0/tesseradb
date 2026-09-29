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
//!   `list<string>` field of the points source itself ([`scan_access_field`]).
//!
//! **Every column is named by the declaration, never by this module** ([`crate::config::Fields`]).
//! A declared field the file does not carry is [`field_index`]'s refusal, and that refusal is the
//! point of the whole arrangement: an absent column reads as empty, and empty is silent in exactly
//! the directions that matter — an absent geometry column puts every point at the origin, and an
//! absent access column puts every point in no principal's mask.
//!
//! **A row is named by its position in its file**, and what it names is the number the rule gave
//! it ([`crate::ids`]): every reader takes a file's [`crate::ids::ReadRows`] and hands its caller
//! each row's number, skipping a row with none. A view's selection, `--limit` and the rows the rule
//! refused are all rows with no number, so no reader here applies any of them again.

use std::collections::{BTreeSet, HashMap};
use std::fs::File;
use std::ops::ControlFlow;
use std::path::Path;


use arrow::array::{Array, UInt32Array, UInt64Array};
use arrow::datatypes::{DataType, TimeUnit};
use tessera_store::access_column::AccessBatch;
use tessera_store::coordinates::{place, read_coordinates, ColumnError};
use arrow::record_batch::RecordBatch;
use parquet::arrow::arrow_reader::ParquetRecordBatchReaderBuilder;
use parquet::file::metadata::ParquetMetaData;
use parquet::file::statistics::Statistics;
use tessera_spatial::tiler::{ScalarType, ScalarValue};
use tessera_spatial::{fixed32, Bounds, Projection};
use tessera_store::scalar_column::ScalarColumn;
use tessera_store::vocabulary::{code_value, Resolved, Unresolved, VocabularyMinter};

use crate::config::{Fields, ViewSelector};
use crate::error::{BuildError, Result};
use crate::ids::{Numbers, ReadRows, NO_SOURCE};
use crate::row_groups::FileGroups;

/// One input point: its number ([`crate::ids`]) and geometry.
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

/// Where the discriminator column sits in a file's own schema, refused by name when it is absent.
///
/// Refused rather than read as *every row*: a form B source with no `view` column would land
/// every one of its rows in every view of the group (`views.md` §3.1).
fn discriminator_index(
    path: &Path,
    schema: &arrow::datatypes::Schema,
    select: &ViewSelector,
) -> Result<usize> {
    schema
        .column_with_name(&select.column)
        .map(|(i, _)| i)
        .ok_or_else(|| BuildError::Schema {
            path: path.to_path_buf(),
            detail: format!(
                "view '{}': `fields.view = \"{}\"` names the column saying which view each row \
                 lands in, and this file has no column of that name. Its columns are: {}",
                select.view_id,
                select.column,
                column_names(schema)
            ),
        })
}

/// Which rows of one decoded batch belong to `select`'s view, and a refusal for any row naming a
/// key the group's roster does not carry (`views.md` §3.1's form B).
///
/// **A stray key is refused, not skipped.** Every other view's rows are skipped here by design —
/// that is what the selection is — so a key nobody declared would be skipped by every view and
/// its rows would vanish from the bundle with nothing said. The refusal names the key and the
/// roster, which is the pair an operator needs to tell a typo from a missing declaration.
///
/// A null discriminator is the same refusal: a row that names no view is in no view.
pub(crate) fn selected_rows(
    path: &Path,
    column: &arrow::array::ArrayRef,
    select: &ViewSelector,
) -> Result<Vec<bool>> {
    use arrow::array::{LargeStringArray, StringArray};
    let stray = |key: Option<&str>| -> BuildError {
        BuildError::Schema {
            path: path.to_path_buf(),
            detail: match key {
                Some(key) => format!(
                    "the discriminator column '{}' carries the key '{key}', which this group's \
                     roster does not list (views §3.1). Its keys are: {}. A row naming a view \
                     nobody declared belongs to no view, and every view's own selection would \
                     skip it — so it is refused here rather than dropped from the bundle in \
                     silence",
                    select.column,
                    select.keys.join(", ")
                ),
                None => format!(
                    "the discriminator column '{}' has a null in it, and a row that names no view \
                     is in no view (views §3.1). Its keys are: {}",
                    select.column,
                    select.keys.join(", ")
                ),
            },
        }
    };
    let rows = column.len();
    let mut keep = vec![false; rows];
    let mut decide = |i: usize, value: Option<&str>| -> Result<()> {
        let Some(value) = value else {
            return Err(stray(None));
        };
        if select
            .keys
            .binary_search_by(|k| k.as_str().cmp(value))
            .is_err()
        {
            return Err(stray(Some(value)));
        }
        keep[i] = value == select.value;
        Ok(())
    };
    if let Some(values) = column.as_any().downcast_ref::<StringArray>() {
        for i in 0..rows {
            decide(i, (!values.is_null(i)).then(|| values.value(i)))?;
        }
        return Ok(keep);
    }
    if let Some(values) = column.as_any().downcast_ref::<LargeStringArray>() {
        for i in 0..rows {
            decide(i, (!values.is_null(i)).then(|| values.value(i)))?;
        }
        return Ok(keep);
    }
    Err(BuildError::Schema {
        path: path.to_path_buf(),
        detail: format!(
            "the discriminator column '{}' has type {:?}, and a view key is a string \
             (views §3.2's charset). Refused rather than coerced: a key read out of another type \
             would select rows for a view under a name nobody wrote",
            select.column,
            column.data_type()
        ),
    })
}

/// One file and the number of each of its rows: everything a reader needs to name a row.
#[derive(Clone, Copy)]
pub struct Source<'a> {
    path: &'a Path,
    fields: &'a Fields,
    rows: &'a ReadRows,
}

/// Every row of a file, row `r` numbered `r`.
static EVERY_ROW: ReadRows = ReadRows {
    numbers: Numbers::Offset { base: 0 },
    groups: None,
    named: 0,
    mixed: 0,
};

impl<'a> Source<'a> {
    pub(crate) fn new(path: &'a Path, fields: &'a Fields, rows: &'a ReadRows) -> Source<'a> {
        Source { path, fields, rows }
    }

    /// Every row of the file, numbered by its position: for a caller reading a file on its own.
    pub fn every_row(path: &'a Path, fields: &'a Fields) -> Source<'a> {
        Source {
            path,
            fields,
            rows: &EVERY_ROW,
        }
    }

    /// The row groups a reader decodes.
    fn groups(&self, groups: &FileGroups) -> Vec<usize> {
        self.rows.groups(groups.count())
    }

    /// The numbers of `len` rows from file row `first`, [`NO_SOURCE`] where a row has none.
    fn source_ids(&self, first: u64, len: usize) -> Vec<u64> {
        let mut ids = Vec::with_capacity(len);
        self.rows.numbers.extend(first, len, &mut ids);
        ids
    }
}

/// Read `points`: every row with a number.
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
    src: Source<'_>,
    projection: Projection,
    extent: &Bounds,
) -> Result<Vec<PointRow>> {
    let mut out = Vec::new();
    scan_points(src, projection, extent, |row| {
        out.push(row);
        ControlFlow::Continue(())
    })?;
    Ok(out)
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
    src: Source<'_>,
    projection: Projection,
    extent: &Bounds,
    mut visit: F,
) -> Result<()> {
    let Source { path, fields, .. } = src;
    let groups = FileGroups::open(path)?;
    let schema = groups.schema().clone();
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
    // Project: the probe corpus carries columns this build has no use for, and at 10^9 rows
    // not decoding them is the difference between one pass and two.
    let mut roots = Vec::with_capacity(3);
    for canonical in geometry_kind.canonical_fields() {
        roots.push(field_index(path, &schema, fields, canonical)?);
    }
    roots.sort_unstable();
    let mask = groups.projection(&roots);
    // Resolved once, on this thread, and copied into each worker: a worker reads its columns
    // under the same names.
    let (x_name, y_name) = (fields.of("x"), fields.of("y"));
    let (morton_name, residual_name) = (fields.of("morton"), fields.of("residual"));

    /// One decoded batch's geometry columns, extracted on a worker thread, with the numbers of
    /// its rows.
    enum PointGeom {
        Xy(Vec<u64>, Vec<f64>, Vec<f64>),
        /// Codes only: 16 bits per axis, all a bare `morton` column can carry.
        Morton(Vec<u64>, Vec<u64>),
        /// Codes plus sub-cell residuals: the full 32 bits per axis.
        MortonResidual(Vec<u64>, Vec<u64>, Vec<u64>),
    }

    let column = |batch: &RecordBatch, name: &str| -> Result<usize> {
        column_index(path, &batch.schema(), name)
    };
    let decode = |first: u64, batch: RecordBatch| -> Result<(u64, PointGeom)> {
        let ids = src.source_ids(first, batch.num_rows());
        let at = |row: usize| format!("row {}", first + row as u64);
        Ok((
            first,
            match geometry_kind {
                GeometryKind::Xy => PointGeom::Xy(
                    ids,
                    read_coordinate_column(path, &batch, column(&batch, x_name)?, x_name, &at)?,
                    read_coordinate_column(path, &batch, column(&batch, y_name)?, y_name, &at)?,
                ),
                GeometryKind::Morton => PointGeom::Morton(
                    ids,
                    read_u64_column(path, &batch, column(&batch, morton_name)?, morton_name)?,
                ),
                GeometryKind::MortonResidual => PointGeom::MortonResidual(
                    ids,
                    read_u64_column(path, &batch, column(&batch, morton_name)?, morton_name)?,
                    read_u64_column(path, &batch, column(&batch, residual_name)?, residual_name)?,
                ),
            },
        ))
    };
    let mut stopped = false;
    groups.decode_parallel(path, &src.groups(&groups), &mask, decode, |(first, geometry)| {
        match geometry {
            PointGeom::Xy(ids, xs, ys) => {
                for i in 0..ids.len() {
                    if ids[i] == NO_SOURCE {
                        continue;
                    }
                    // Counted by the survey every build runs before this pass.
                    let placed = place(projection, Some(extent), xs[i], ys[i]).map_err(|e| {
                        BuildError::Schema {
                            path: path.to_path_buf(),
                            detail: format!("row {} {e}", first + i as u64),
                        }
                    })?;
                    if visit(PointRow {
                        source_id: ids[i],
                        qx: fixed32(placed.x, extent.x_min, extent.x_max),
                        qy: fixed32(placed.y, extent.y_min, extent.y_max),
                    })
                    .is_break()
                    {
                        stopped = true;
                        return Ok(ControlFlow::Break(()));
                    }
                }
            }
            PointGeom::Morton(ids, codes) => {
                for i in 0..ids.len() {
                    if ids[i] == NO_SOURCE {
                        continue;
                    }
                    let code = narrow_code(path, codes[i], "morton")?;
                    let (cx, cy) = deinterleave(code);
                    // A bare `morton` column holds 16 bits per axis and no more, so the
                    // sub-cell position is *zero*, not unknown: the point sits at its cell's
                    // origin.
                    if visit(PointRow {
                        source_id: ids[i],
                        qx: (cx as u32) << 16,
                        qy: (cy as u32) << 16,
                    })
                    .is_break()
                    {
                        stopped = true;
                        return Ok(ControlFlow::Break(()));
                    }
                }
            }
            PointGeom::MortonResidual(ids, codes, residuals) => {
                for i in 0..ids.len() {
                    if ids[i] == NO_SOURCE {
                        continue;
                    }
                    let code = narrow_code(path, codes[i], "morton")?;
                    let residual = narrow_code(path, residuals[i], "residual")?;
                    let (cx, cy) = deinterleave(code);
                    let (rx, ry) = deinterleave(residual);
                    // Reassembly, not conversion: the file already holds the 32-bit fixed-point
                    // position, split across two words in the same axis convention this build
                    // stores it in.
                    if visit(PointRow {
                        source_id: ids[i],
                        qx: ((cx as u32) << 16) | rx as u32,
                        qy: ((cy as u32) << 16) | ry as u32,
                    })
                    .is_break()
                    {
                        stopped = true;
                        return Ok(ControlFlow::Break(()));
                    }
                }
            }
        }
        Ok(ControlFlow::Continue(()))
    })?;
    let _ = stopped;
    Ok(())
}

/// Read `pairs` (`entity_id`, `term_id`), keeping rows with `entity_id < limit`, grouped into
/// each source entity's term list. Lists are returned sorted and deduplicated: the label set is
/// a *set*, and downstream (the signature key, the postings writer) depends on it being one.
pub fn read_pairs(src: Source<'_>) -> Result<HashMap<u64, Vec<u64>>> {
    let mut grouped: HashMap<u64, Vec<u64>> = HashMap::new();
    scan_pairs(src, |source_id, term| {
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
    src: Source<'_>,
    mut visit: F,
) -> Result<()> {
    let Source { path, fields, .. } = src;
    let groups = FileGroups::open(path)?;
    let term_idx = field_index(path, groups.schema(), fields, TERM_ID)?;
    let term_name = fields.of(TERM_ID);
    let projection = groups.projection(&[term_idx]);
    let decode = |first: u64, batch: RecordBatch| -> Result<(Vec<u64>, Vec<u64>)> {
        let ids = src.source_ids(first, batch.num_rows());
        let terms = read_u64_column(path, &batch, 0, term_name)?;
        Ok((ids, terms))
    };
    groups.decode_parallel(path, &src.groups(&groups), &projection, decode, |(ids, terms)| {
        for i in 0..ids.len() {
            if ids[i] == NO_SOURCE {
                continue;
            }
            if visit(ids[i], terms[i]).is_break() {
                return Ok(ControlFlow::Break(()));
            }
        }
        Ok(ControlFlow::Continue(()))
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

/// The distinct access terms a field-sourced view carries, sorted, with the default among them
/// where one is declared — and beside them, how many rows carry a null or empty label.
///
/// **A whole pass over one column before any term id exists**, which is the price of assigning
/// term ids by a rule both builds can compute: the linear build walks items and interns as it
/// goes, and the streaming build ranks terms by `(first ordinal, source term)` over a relation it
/// scans twice. Making the source term a position in a *sorted* list is what makes those two the
/// same ordering. The relation route pays nothing for this — its source terms are already integers
/// the file supplies.
///
/// A declared default is always present, because it is what a null or empty row is filled with
/// and a fill must have a term to fill with. The unlabelled count is what the caller refuses on
/// where no default is declared (decision 0133): it is counted here, before any term id exists
/// and before anything is written, so the refusal names an exact number and costs no output.
pub fn read_access_vocabulary(
    src: Source<'_>,
    field: Option<&str>,
    default: Option<&str>,
) -> Result<(Vec<String>, u64)> {
    let mut distinct: BTreeSet<String> = BTreeSet::new();
    if let Some(default) = default {
        distinct.insert(default.to_string());
    }
    let mut unlabelled: u64 = 0;
    if let Some(field) = field {
        // **Marked per row, inserted per distinct value.** A row contributes a bit to `used` and
        // nothing else; the set takes the batch's distinct values once, and only those some
        // selected row carries — a dictionary page holds the row group's values, not this
        // prefix's, and a `--limit` build must not mint a term no row it read carried.
        let mut used: Vec<bool> = Vec::new();
        scan_access_column(src, field, |_, rows, batch| {
            used.clear();
            used.resize(batch.distinct().len(), false);
            for &row in rows {
                let terms = batch.row(row as usize);
                if terms.is_empty() {
                    unlabelled += 1;
                }
                for &term in terms {
                    used[term as usize] = true;
                }
            }
            for (term, &carried) in used.iter().enumerate() {
                if carried && !distinct.contains(batch.distinct()[term]) {
                    distinct.insert(batch.distinct()[term].to_string());
                }
            }
            ControlFlow::Continue(())
        })?;
    }
    Ok((distinct.into_iter().collect(), unlabelled))
}

/// The field route's counterpart to [`scan_pairs`]: one `(source_id, source_term)` per term a
/// point carries, and **one carrying the default for a point that carries none**.
///
/// Three rules, all of them decided here because this is where a row's value becomes a term:
///
/// - **A null value and an empty list both mean *no access terms*.** Neither means unrestricted.
///   Where the view declares a default those rows get exactly it, and where it declares one that
///   no principal holds they stay invisible. Where it declares none the corpus was refused at the
///   vocabulary pass (decision 0133), so a row reaching this scan with no terms and no default is
///   a file that changed underneath the build. The permissive misreading — *null is unspecified,
///   so unrestricted* — would put every unlabelled point in everyone's mask.
/// - **Terms are trimmed** by the label rule every reader of labels applies
///   ([`tessera_types::label`]), so ` cs.LG` and `cs.LG` are one term. A term that is empty after
///   trimming is not a term.
/// - **Filling never overrides.** A point carrying terms of its own keeps exactly those. A point's
///   terms are disjunctive — `M_auth` is a union of posting lists — so a label added to a point can
///   only widen it, which makes overriding inadmissible rather than merely unwise.
///
/// `field` is `None` for a view declaring only a default, where every point takes it; a view
/// declaring neither is refused at the declaration and at [`crate::plan_access`].
// The eighth argument is the view's selection, and it belongs beside the file it filters: every
// pass over a form B source takes the same three (`path`, `fields`, `select`) and a struct around
// them would be a second spelling of `ViewArgs`.
pub fn scan_access_field<F: FnMut(u64, u64) -> ControlFlow<()>>(
    src: Source<'_>,
    field: Option<&str>,
    vocabulary: &[String],
    default_term: Option<u64>,
    mut visit: F,
) -> Result<AccessFill> {
    let points = src.path;
    let mut fill = AccessFill::default();
    let Some(field) = field else {
        let Some(default_term) = default_term else {
            return Err(BuildError::Invalid(format!(
                "{}: `point_visibility` names no field, no source and no default, so no point \
                 has a label (decision 0133)",
                points.display()
            )));
        };
        // Every point takes the default: the corpus with no permission model. Read from the
        // identity column alone, so a view declaring only a default opens no access column at all.
        scan_identity(src, |source_id| {
            fill.filled += 1;
            visit(source_id, default_term)
        })?;
        return Ok(fill);
    };
    // A term this pass sees and the vocabulary pass did not means the file changed underneath the
    // build. Refused rather than assumed away: the two passes must see one relation, and the
    // second is what assigns the postings.
    let mut changed: Option<BuildError> = None;
    // **One search a distinct value a batch**, not one a row: `u64::MAX` stands for a value this
    // batch has not needed yet, and a value no selected row carries is never looked up at all —
    // which is what lets a dictionary page hold values this prefix does not read.
    let mut positions: Vec<u64> = Vec::new();
    scan_access_column(src, field, |ids, rows, batch| {
        positions.clear();
        positions.resize(batch.distinct().len(), u64::MAX);
        for &row in rows {
            let source_id = ids[row as usize];
            let terms = batch.row(row as usize);
            if terms.is_empty() {
                let Some(default_term) = default_term else {
                    changed = Some(BuildError::Schema {
                        path: points.to_path_buf(),
                        detail: format!(
                            "the access column '{field}' now carries a null or empty label, which \
                             it did not when this build read its vocabulary, and the view declares \
                             no `point_visibility.default` to fill it with. The file changed \
                             underneath the build, and the two passes must see one relation"
                        ),
                    });
                    return ControlFlow::Break(());
                };
                fill.filled += 1;
                if visit(source_id, default_term).is_break() {
                    return ControlFlow::Break(());
                }
                continue;
            }
            fill.carried += 1;
            for &term in terms {
                let mut position = positions[term as usize];
                if position == u64::MAX {
                    let value = batch.distinct()[term as usize];
                    let Ok(at) = vocabulary.binary_search_by(|t| t.as_str().cmp(value)) else {
                        changed = Some(BuildError::Schema {
                            path: points.to_path_buf(),
                            detail: format!(
                                "the access column '{field}' now carries the term '{value}', \
                                 which it did not when this build read its vocabulary. The file \
                                 changed underneath the build, and the two passes must see one \
                                 relation"
                            ),
                        });
                        return ControlFlow::Break(());
                    };
                    position = at as u64;
                    positions[term as usize] = position;
                }
                if visit(source_id, position).is_break() {
                    return ControlFlow::Break(());
                }
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

/// Every row with a number, in file order, with nothing decoded: the numbers are the rows.
fn scan_identity<F: FnMut(u64) -> ControlFlow<()>>(src: Source<'_>, mut visit: F) -> Result<()> {
    let groups = FileGroups::open(src.path)?;
    for group in src.groups(&groups) {
        let first = groups.start(group);
        let len = groups.group_rows(group) as usize;
        for id in src.source_ids(first, len) {
            if id == NO_SOURCE {
                continue;
            }
            if visit(id).is_break() {
                return Ok(());
            }
        }
    }
    Ok(())
}

/// Walk the access column, handing the caller **one batch at a time**: its distinct values, and
/// each selected row's indices into them.
///
/// **A batch and not a row, because the column is a category.** The access column of a corpus
/// this size names a compartment — 251 country codes over 3.5×10⁹ GBIF occurrences — and Parquet
/// stores such a column dictionary-encoded, one page of values and a run of indices. The pass
/// this replaced hydrated that back into one `String` a row and then asked the caller to look
/// each one up: measured at the 3×10⁸ prefix, 14.8 s of allocation and 30.5 s of lookup in a
/// 65.6 s `dictionary` stage, and the same again in `geometry_read`, which runs the second pass.
/// Here the reader is asked for the dictionary itself ([`ArrowReaderOptions::with_schema`]), the
/// distinct values are trimmed once a batch, and a row is an index.
///
/// A column the reader will not hand over as a dictionary — a `list<string>`, or a file whose
/// pages are plain — falls back to building the same shape with a hash per row, which is still
/// one allocation a *value* rather than one a row.
fn scan_access_column<F: FnMut(&[u64], &[u32], &AccessBatch<'_>) -> ControlFlow<()>>(
    src: Source<'_>,
    field: &str,
    mut visit: F,
) -> Result<()> {
    let Source { path, fields, .. } = src;
    let plain = FileGroups::open(path)?;
    // The access field is named directly by `point_visibility.field` rather than through the map:
    // it is the declaration, not a relocation of a canonical name. Absent is the same refusal a
    // moved name gets, and for the same reason: an unread access column is a corpus in no
    // principal's mask, silently.
    let access_root = plain
        .schema()
        .column_with_name(field)
        .map(|(i, _)| i)
        .ok_or_else(|| BuildError::Schema {
            path: path.to_path_buf(),
            detail: format!(
                "{}: `point_visibility.field = \"{field}\"` names a column this file does not \
                 carry. Its columns are: {}. Refused rather than read as empty: with no access \
                 column read, every point would carry no term and so sit in no principal\'s mask",
                fields.object(),
                column_names(plain.schema())
            ),
        })?;
    // **Ask for the dictionary, take the strings if it is refused.** A schema asking for the
    // column as a dictionary is checked against the file's own types, so a column that is not a
    // plain string, a `list<string>`, fails here and the plain footer reads it. Nothing downstream
    // sees which ran: the batch handed to the visitor has one shape.
    let groups = access_groups(path, plain.schema(), field).unwrap_or(plain);
    let projection = groups.projection(&[access_root]);
    let kept = src.groups(&groups);
    let mut rows: Vec<u32> = Vec::with_capacity(ATTRIBUTE_BATCH_ROWS);
    groups.each_batch(path, &kept, &projection, |first, batch| {
        let source_ids = src.source_ids(first, batch.num_rows());
        let terms = read_access_column(path, batch.column(0), field)?;
        rows.clear();
        rows.extend(
            source_ids
                .iter()
                .enumerate()
                .filter(|(_, &id)| id != NO_SOURCE)
                .map(|(row, _)| row as u32),
        );
        Ok(visit(&source_ids, &rows, &terms))
    })
}

/// The access column's footer asking for the column as a dictionary rather than as hydrated
/// strings, or `None` where the file's type does not allow it.
fn access_groups(
    path: &Path,
    schema: &arrow::datatypes::SchemaRef,
    field: &str,
) -> Option<FileGroups> {
    use arrow::datatypes::{Field, Schema};
    use parquet::arrow::arrow_reader::ArrowReaderOptions;
    use std::sync::Arc;
    let fields: Vec<Arc<Field>> = schema
        .fields()
        .iter()
        .map(|f| {
            if f.name() == field && crate::utf8::is_utf8(f.data_type()) {
                Arc::new(Field::new(
                    f.name(),
                    DataType::Dictionary(Box::new(DataType::Int32), Box::new(DataType::Utf8)),
                    f.is_nullable(),
                ))
            } else {
                f.clone()
            }
        })
        .collect();
    let options = ArrowReaderOptions::new().with_schema(Arc::new(Schema::new(fields)));
    FileGroups::open_with(path, options).ok()
}

/// Each row's access labels, read as a points file's access column is read. An artifact source's
/// label column takes this.
pub(crate) fn access_labels(
    path: &Path,
    column: &arrow::array::ArrayRef,
    name: &str,
) -> Result<Vec<Vec<String>>> {
    let batch = read_access_column(path, column, name)?;
    Ok((0..column.len())
        .map(|row| batch.labels(row).map(str::to_string).collect())
        .collect())
}

/// [`tessera_store::access_column::read_access_column`], refused as a schema error on `path`.
fn read_access_column<'a>(
    path: &Path,
    column: &'a arrow::array::ArrayRef,
    name: &str,
) -> Result<AccessBatch<'a>> {
    tessera_store::access_column::read_access_column(column, name).map_err(|detail| {
        BuildError::Schema {
            path: path.to_path_buf(),
            detail,
        }
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
    /// Rows the build would place: where the survey is given the rows kept, those.
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

/// The rows of one view's points the identity pass kept.
#[derive(Debug, Clone, Copy)]
pub struct KeptRows<'a>(pub(crate) &'a crate::ids::ReadRows);

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
/// `kept` is the rows the identity pass kept, where it has run: under `--limit` the extent frames
/// those rows, since a prefix build whose box was computed over the whole file would quantise its
/// rows into a fraction of the grid, and a row naming a kept item is kept whatever its own value.
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
    kept: Option<KeptRows<'_>>,
    select: Option<&ViewSelector>,
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

    let mut roots = Vec::with_capacity(3);
    for canonical in ["x", "y"] {
        roots.push(field_index(path, &schema, fields, canonical)?);
    }
    // The box is this view's own, so the survey reads this view's rows (`views.md` §3.1): a
    // group's frame is fitted over every view's source at once, and each of those surveys sees
    // only the rows the discriminator gives it.
    if let Some(select) = select {
        roots.push(discriminator_index(path, &schema, select)?);
    }
    roots.sort_unstable();
    roots.dedup();
    drop(builder);
    let groups = FileGroups::open(path)?;
    let mask = groups.projection(&roots);
    let keep = match kept {
        Some(kept) => kept.0.groups(groups.count()),
        None => (0..groups.count()).collect(),
    };

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
    let mut numbers: Vec<u64> = Vec::new();
    groups.each_batch(path, &keep, &mask, |first, batch| {
        let column = |name: &str| column_index(path, &batch.schema(), name);
        let (x_idx, y_idx) = (column(x_name)?, column(y_name)?);
        numbers.clear();
        if let Some(kept) = kept {
            kept.0.numbers.extend(first, batch.num_rows(), &mut numbers);
        }
        let name_of = |i: usize| format!("row {}", first + i as u64);
        let xs = read_coordinate_column(path, &batch, x_idx, x_name, &name_of)?;
        let ys = read_coordinate_column(path, &batch, y_idx, y_name, &name_of)?;
        let keep = match select {
            Some(select) => Some(selected_rows(
                path,
                batch.column(column(&select.column)?),
                select,
            )?),
            None => None,
        };
        for i in 0..xs.len() {
            if numbers.get(i).is_some_and(|&number| number == crate::ids::NO_SOURCE)
                || !keep.as_ref().is_none_or(|keep| keep[i])
            {
                continue;
            }
            let placed = place(projection, against, xs[i], ys[i]).map_err(|e| BuildError::Schema {
                path: path.to_path_buf(),
                detail: format!("{} {e}", name_of(i)),
            })?;
            let (x, y) = placed.projected;
            survey.clipped += u64::from(placed.clipped);
            survey.clamped_x += u64::from(placed.clamped_x);
            survey.clamped_y += u64::from(placed.clamped_y);
            survey.clamped += u64::from(placed.clamped());
            found = true;
            survey.rows += 1;
            x_min = x_min.min(x);
            x_max = x_max.max(x);
            y_min = y_min.min(y);
            y_max = y_max.max(y);
        }
        Ok(ControlFlow::Continue(()))
    })?;
    survey.bounds = found.then_some(Bounds {
        x_min,
        x_max,
        y_min,
        y_max,
    });
    Ok(PointSurvey::Coordinates(survey))
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
    /// The canonical fields this shape reads — the list `fields` is resolved against to build the
    /// projection.
    fn canonical_fields(self) -> &'static [&'static str] {
        match self {
            GeometryKind::Xy => &["x", "y"],
            GeometryKind::Morton => &["morton"],
            GeometryKind::MortonResidual => &["morton", "residual"],
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

/// Row groups worth reading: those whose statistics for the column at `id_idx` do not prove
/// every row is `>= limit`. Returns *all* row groups when there is no limit or no usable statistic — the
/// filter is an optimisation and must never drop a row it cannot prove is excluded.
pub(crate) fn prunable_row_groups(
    meta: &ParquetMetaData,
    id_idx: usize,
    limit: Option<u64>,
) -> Vec<usize> {
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
        Statistics::Int32(s) => s.min_opt().and_then(|v| u64::try_from(*v).ok()),
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

/// The first of `names` this file carries, from the Parquet footer alone.
pub(crate) fn first_column_present(path: &Path, names: &[&str]) -> Result<Option<String>> {
    let file = File::open(path).map_err(|e| BuildError::io(path, e))?;
    let builder =
        ParquetRecordBatchReaderBuilder::try_new(file).map_err(|e| BuildError::parquet(path, e))?;
    let schema = builder.schema();
    Ok(names
        .iter()
        .find(|name| schema.column_with_name(name).is_some())
        .map(|name| (*name).to_string()))
}

/// An integer column as `u64`s, a null staying null, or `None` where the column is not an integer.
/// A signed value is sign-extended to 64 bits and its bits read as unsigned.
fn integer_ids(column: &dyn Array) -> Option<UInt64Array> {
    use arrow::array::{Int16Array, Int32Array, Int64Array, Int8Array, UInt16Array, UInt8Array};
    let any = column.as_any();
    if let Some(ids) = any.downcast_ref::<UInt8Array>() {
        return Some(ids.unary(u64::from));
    }
    if let Some(ids) = any.downcast_ref::<UInt16Array>() {
        return Some(ids.unary(u64::from));
    }
    if let Some(ids) = any.downcast_ref::<UInt32Array>() {
        return Some(ids.unary(u64::from));
    }
    if let Some(ids) = any.downcast_ref::<UInt64Array>() {
        return Some(ids.clone());
    }
    if let Some(ids) = any.downcast_ref::<Int8Array>() {
        return Some(ids.unary(|id| i64::from(id) as u64));
    }
    if let Some(ids) = any.downcast_ref::<Int16Array>() {
        return Some(ids.unary(|id| i64::from(id) as u64));
    }
    if let Some(ids) = any.downcast_ref::<Int32Array>() {
        return Some(ids.unary(|id| i64::from(id) as u64));
    }
    if let Some(ids) = any.downcast_ref::<Int64Array>() {
        return Some(ids.unary(|id| id as u64));
    }
    None
}

/// An integer column, or a list column of them, as `u64`s, a null staying null. A signed value is
/// sign-extended to 64 bits and its bits read as unsigned. For a list column this is its items,
/// every row's together.
pub(crate) fn id_values(path: &Path, column: &dyn Array, name: &str) -> Result<UInt64Array> {
    let list = column.as_any().downcast_ref::<arrow::array::ListArray>();
    let values = list.map_or(column, |list| list.values().as_ref());
    integer_ids(values).ok_or_else(|| {
        let shape = if list.is_some() { "a list of " } else { "" };
        BuildError::Schema {
            path: path.to_path_buf(),
            detail: format!(
                "column '{name}' is {}; write it as {shape}an integer: uint8 to uint64 or int8 \
                 to int64",
                column.data_type()
            ),
        }
    })
}

/// Read an integer column that is not an identity as `u64`, accepting the widths a Parquet writer
/// may have chosen and refusing a negative value.
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
            // `.values()` underneath a null yields an arbitrary value silently. Nulls were checked
            // on the source column above; check again after the cast so a negative value is a
            // schema error, never a wrong one.
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

/// A coordinate column, read by [`read_coordinates`]. `row_name` says how a refusal names a row of
/// `batch`.
fn read_coordinate_column(
    path: &Path,
    batch: &RecordBatch,
    idx: usize,
    name: &str,
    row_name: &dyn Fn(usize) -> String,
) -> Result<Vec<f64>> {
    read_coordinates(batch.column(idx).as_ref()).map_err(|e| {
        let at = match e {
            ColumnError::Null { row } => format!("{}: ", row_name(row)),
            ColumnError::Type(_) => String::new(),
        };
        BuildError::Schema {
            path: path.to_path_buf(),
            detail: format!("{at}column '{name}' {e}"),
        }
    })
}

/// Read a vocabulary file: `key`, an **optional** `code`, and an optional `title`.
///
/// **Parquet, like every other build input**, so a 400-value published vocabulary is the same
/// kind of artifact as the points and pairs files and needs no second reader.
///
/// **`code` may be absent, and then the build draws one** (`configuration.md` §1): a sourced
/// value set is the same pair of choices an inline one is — where the values come from,
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
    // design (absence draws a code, and a value may have no title). A *declared* `code` or
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
        let keys =
            crate::utf8::Utf8Column::new(batch.column(key_idx).as_ref()).ok_or_else(|| {
                BuildError::Schema {
                    path: path.to_path_buf(),
                    detail: format!("vocabulary column '{}' must be utf8", fields.of("key")),
                }
            })?;
        let code_values = match code_idx {
            Some(idx) => Some(read_u64_column(path, &batch, idx, fields.of("code"))?),
            None => None,
        };
        let title_values = match title_idx {
            Some(idx) => Some(
                crate::utf8::Utf8Column::new(batch.column(idx).as_ref()).ok_or_else(|| {
                    BuildError::Schema {
                        path: path.to_path_buf(),
                        detail: format!("vocabulary column '{}' must be utf8", fields.of("title")),
                    }
                })?,
            ),
            None => None,
        };
        for row in 0..batch.num_rows() {
            let key = keys.value(row).to_string();
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
/// in three files calls this three times, each over the columns that named that file. A category's
/// key is resolved against the whole declaration rather than against the file it arrived in, and
/// that resolution is [`BatchColumn::value`]'s — the caller's, now that it holds the batch.
///
/// **A second pass over the points file rather than a widening of [`scan_points`].** [`PointRow`]
/// is a 16-byte `Copy` struct held one per entity by both builds, and its doc argues that width;
/// a variable-length attribute tail hung off it would make the build's one per-entity structure
/// grow with the schema. The two passes are independent.
///
/// **A batch is handed over whole, not a row at a time, because the per-row work is the pass and
/// the Parquet decode is not.** Measured on GeoNames (13,463,857 rows, thirteen declared columns):
/// of the 17.6 s the attribute pass took, the Parquet reader was **1.6 s** and
/// [`BatchColumn::decode`] 0.04 s; the remaining ~16 s was [`BatchColumn::value`], the staging
/// write beside it and the scatter after it — all of it per row *per column*, and all of it
/// independent between columns. A row-shaped handover cannot be split that way, so the caller is
/// given the decoded batch and splits it itself
/// ([`crate::pipeline::read_attributes_by_entity`]). Decode stays serial and in file order, which
/// is what keeps the mint pre-pass below deterministic.
///
/// Category keys are mapped to codes against the declaration's compiled vocabularies. Under a
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
pub fn scan_attributes<F: FnMut(AttributeBatch<'_>) -> Result<()>>(
    src: Source<'_>,
    columns: &[&crate::config::Attribute],
    minters: &mut HashMap<String, VocabularyMinter>,
    mut visit: F,
) -> Result<()> {
    if columns.is_empty() {
        return Ok(());
    }
    let Source { path, fields, .. } = src;
    // A unique attribute sits where this file's map moved it; any other in its own column.
    let named: Vec<&str> = columns
        .iter()
        .map(|attribute| {
            fields
                .unique_column(&attribute.name)
                .unwrap_or(attribute.column())
        })
        .collect();
    let groups = FileGroups::open(path)?;
    let file_schema = groups.schema().clone();
    tessera_store::declaration::check_declared_present(
        columns
            .iter()
            .zip(&named)
            .map(|(attribute, column)| (attribute.name.as_str(), *column)),
        |column| file_schema.column_with_name(column).is_some(),
    )
    .map_err(|detail| BuildError::Schema {
        path: path.to_path_buf(),
        detail: format!("{detail} (its columns are {})", column_names(&file_schema)),
    })?;
    let mut roots: Vec<usize> = Vec::with_capacity(columns.len());
    for column in &named {
        let (index, _) = file_schema
            .column_with_name(column)
            .expect("every declared column was found above");
        if !roots.contains(&index) {
            roots.push(index);
        }
    }
    roots.sort_unstable();
    let projection = groups.projection(&roots);
    let mut rows: Vec<u32> = Vec::with_capacity(ATTRIBUTE_BATCH_ROWS);
    groups.each_batch(path, &src.groups(&groups), &projection, |first, batch| {
        let ids = src.source_ids(first, batch.num_rows());
        // **Decoded once per batch, not once per row.** An earlier revision called a
        // whole-column converter from inside the row loop, so a 65,536-row batch decoded its
        // integer columns 65,536 times — quadratic in the batch size, and invisible at the scale
        // a test uses. A discovered category's mint pre-pass rides the same discipline: minting
        // is per distinct key in the batch, decided here, not per row.
        let mut decoded: Vec<BatchColumn> = Vec::with_capacity(columns.len());
        for (attribute, column) in columns.iter().zip(&named) {
            let column = batch
                .column_by_name(column)
                .expect("every declared column is projected");
            decoded.push(BatchColumn::decode(path, column, attribute, minters)?);
        }
        rows.clear();
        rows.extend(
            ids.iter()
                .enumerate()
                .filter(|(_, &id)| id != NO_SOURCE)
                .map(|(row, _)| row as u32),
        );
        visit(AttributeBatch {
            ids: &ids,
            rows: &rows,
            decoded: &decoded,
        })?;
        Ok(ControlFlow::Continue(()))
    })
}

/// The refusal for `items` items this build created that have no row in the attribute source at
/// `path`, which carries `columns`, or `None` where it owes them none.
///
/// Every item has a row in each source, as a new item at a running service carries every declared
/// column, and a row of nulls is how a source says it has no value. A unique field is the
/// exception: its values come from every file carrying its column, so an item another file created
/// with its value, or without one, owes the field's own source no row.
pub(crate) fn items_without_a_row(
    path: &Path,
    columns: &[&crate::config::Attribute],
    items: u64,
) -> Option<BuildError> {
    let owed: Vec<&crate::config::Attribute> =
        columns.iter().copied().filter(|a| !a.unique).collect();
    if items == 0 || owed.is_empty() {
        return None;
    }
    let detail = tessera_store::declaration::check_declared_present(
        owed.iter()
            .map(|attribute| (attribute.name.as_str(), attribute.column())),
        |_| false,
    )
    .expect_err("an attribute source carries at least one column");
    Some(BuildError::Schema {
        path: path.to_path_buf(),
        detail: format!("{items} item(s) have no row: {detail}"),
    })
}

/// How many rows one decoded batch of an attribute source carries — the Parquet reader's batch
/// size, and the bound a caller's staging buffer must leave room for above its own budget.
pub const ATTRIBUTE_BATCH_ROWS: usize = 65_536;

/// One decoded batch of an attribute source, handed to [`scan_attributes`]'s visitor whole.
///
/// **The unit of the handover is a batch because the unit of the work is a column.** Every
/// expensive thing the caller does with a row — resolving its value, staging it, scattering it
/// into entity order — it does once per declared column, and the columns share nothing mutable;
/// a row-shaped visit forces all of that onto one thread. See [`scan_attributes`] for the
/// measurement that says so.
pub struct AttributeBatch<'a> {
    /// The batch's numbers, indexed by the values in [`Self::rows`].
    pub ids: &'a [u64],
    /// The rows of this batch that name an item, ascending.
    pub rows: &'a [u32],
    /// One decoded column per declared attribute of this source, in the caller's `columns` order.
    pub decoded: &'a [BatchColumn],
}

/// One batch's worth of a declared column, decoded to the shape the row loop indexes.
///
/// A category keeps absence in band as the reserved code 0; every other declaration is read by
/// [`tessera_store::scalar_column`], the rule an ingest batch is read by.
pub struct BatchColumn {
    values: BatchValues,
}

enum BatchValues {
    /// Category keys under a **declared** vocabulary, resolved per row: `value` looks each key up
    /// against `schema_decl` and refuses an unknown one.
    Keys(crate::utf8::Utf8Values),
    /// Category codes under a **discovered** vocabulary, already resolved by the batch-level mint
    /// pre-pass in `decode`, so `value` is a pure index.
    Discovered(Vec<u32>),
    Scalar(ScalarColumn),
}

impl BatchColumn {
    fn decode(
        path: &Path,
        column: &arrow::array::ArrayRef,
        attribute: &crate::config::Attribute,
        minters: &mut HashMap<String, VocabularyMinter>,
    ) -> Result<Self> {
        Ok(BatchColumn {
            values: Self::decode_values(path, column, attribute, minters)?,
        })
    }

    fn decode_values(
        path: &Path,
        column: &arrow::array::ArrayRef,
        attribute: &crate::config::Attribute,
        minters: &mut HashMap<String, VocabularyMinter>,
    ) -> Result<BatchValues> {
        if let Some(vocabulary) = &attribute.vocabulary {
            // A category arrives as its *key*, never as a code: §3.1 — the key in the row is not
            // the display name, and the code is drawn once and pinned, so a data file
            // supplying codes directly would be a second place codes are decided.
            let keys = tessera_store::scalar_column::category_keys(column).ok_or_else(|| {
                BuildError::Schema {
                    path: path.to_path_buf(),
                    detail: format!(
                        "attribute '{}' is a category and this file holds {:?}; write its value \
                         keys as utf8",
                        attribute.name,
                        column.data_type()
                    ),
                }
            })?;
            return match attribute.value_set {
                Some(crate::config::ValueSet::Open) => {
                    let minter = minters.get_mut(vocabulary).ok_or_else(|| {
                        BuildError::Invalid(format!(
                            "attribute '{}' draws codes from open vocabulary '{vocabulary}', and \
                             the scan was given no minter for it; pass `Schema::open_minters`",
                            attribute.name
                        ))
                    })?;
                    Ok(BatchValues::Discovered(mint_batch(
                        keys.column(),
                        minter,
                        attribute,
                    )?))
                }
                // Declared (or a vocabulary shared by naming it): resolved per row in `value`,
                // unchanged from the declare-then-use rule.
                _ => Ok(BatchValues::Keys(keys)),
            };
        }
        ScalarColumn::new(column, attribute.ty)
            .map(BatchValues::Scalar)
            .ok_or_else(|| BuildError::Schema {
                path: path.to_path_buf(),
                detail: format!(
                    "attribute '{}' is declared '{}' and the points file holds {:?}, which cannot \
                     carry it; convert the column or change the declaration{}",
                    attribute.name,
                    attribute.ty.arrow_type_name(),
                    column.data_type(),
                    match column.data_type() {
                        DataType::Timestamp(unit, _) if *unit != TimeUnit::Microsecond => {
                            " (a timestamp is read only in microseconds, so cast it to \
                             timestamp[us])"
                        }
                        _ => "",
                    }
                ),
            })
    }

    pub fn value(
        &self,
        row: usize,
        attribute: &crate::config::Attribute,
        schema_decl: &crate::config::Schema,
    ) -> Result<ScalarValue> {
        Ok(match &self.values {
            BatchValues::Keys(keys) => {
                let vocabulary = attribute
                    .vocabulary
                    .as_ref()
                    .expect("a Keys column belongs to a category");
                let resolved = schema_decl.vocabularies[vocabulary]
                    .values
                    .resolve(keys.column().at(row))
                    .map_err(|e| unresolved(attribute, e))?;
                match resolved {
                    Resolved::Code(code) => code_value(attribute.ty, code),
                    Resolved::Novel(_) => unreachable!("a closed vocabulary lists every key"),
                }
            }
            // Already resolved by `decode`'s mint pre-pass — a pure index, like every other
            // variant here, and no lookup against `schema_decl` at all.
            BatchValues::Discovered(codes) => code_value(attribute.ty, codes[row]),
            BatchValues::Scalar(column) => column.value(row).map_err(|e| {
                crate::config::declaration_error(format!(
                    "attribute '{}' (declared '{}'): the points file carries {e}; declare a wider \
                     type and rebuild",
                    attribute.name,
                    attribute.ty.arrow_type_name(),
                ))
            })?,
        })
    }
}

/// A category cell's refusal, naming the attribute.
fn unresolved(attribute: &crate::config::Attribute, e: Unresolved) -> BuildError {
    crate::config::declaration_error(format!("attribute '{}' {e}", attribute.name))
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
/// Each cell is resolved by [`VocabularyMinter::resolve`], so an empty key is refused here as it is
/// at a running service.
fn mint_batch(
    keys: crate::utf8::Utf8Column<'_>,
    minter: &mut VocabularyMinter,
    attribute: &crate::config::Attribute,
) -> Result<Vec<u32>> {
    use std::collections::BTreeSet;

    let mut novel: BTreeSet<&str> = BTreeSet::new();
    for i in 0..keys.len() {
        let resolved = minter
            .resolve(keys.at(i))
            .map_err(|e| unresolved(attribute, e))?;
        if let Resolved::Novel(key) = resolved {
            novel.insert(key);
        }
    }
    for key in novel {
        minter.mint(key).map_err(|e| {
            crate::config::declaration_error(format!("attribute '{}': {e}", attribute.name))
        })?;
    }

    Ok((0..keys.len())
        .map(|i| match minter.resolve(keys.at(i)) {
            Ok(Resolved::Code(code)) => code,
            _ => unreachable!("every key in this batch was just minted or was already bound"),
        })
        .collect())
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
            Int8Array, LargeStringArray, StringArray, TimestampMicrosecondArray, UInt16Array,
            UInt32Array, UInt64Array, UInt8Array,
        };
        use arrow::datatypes::TimeUnit;
        use std::sync::Arc;
        use tessera_store::scalar_column;

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
                unique: false,
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
            // The same bytes at 64-bit offsets, which is what pandas 3 writes.
            Arc::new(LargeStringArray::from(vec!["k"])),
            Arc::new(TimestampMicrosecondArray::from(vec![1i64])),
            // The near miss the decoder names outright: a timestamp in the wrong unit.
            Arc::new(arrow::array::TimestampMillisecondArray::from(vec![1i64])),
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
                if vocabulary.is_some()
                    && !matches!(ty, ScalarType::U8 | ScalarType::U16 | ScalarType::U32)
                {
                    continue;
                }
                let attribute = attribute(ty, vocabulary);
                for column in &columns {
                    let mut minters = HashMap::new();
                    let decoded =
                        BatchColumn::decode_values(path, column, &attribute, &mut minters).is_ok();
                    assert_eq!(
                        scalar_column::carries(ty, vocabulary.is_some(), column.data_type()),
                        decoded,
                        "declared {ty:?} (vocabulary {vocabulary:?}) against {:?}",
                        column.data_type()
                    );
                }
            }
        }
        // And the unit that must not pass, stated outright rather than left to the loop.
        assert!(!scalar_column::carries(
            ScalarType::TimestampUs,
            false,
            &DataType::Timestamp(TimeUnit::Millisecond, None)
        ));
    }

    /// A roster integer is held as an `i64`, so a `uint64` past `i64::MAX` is refused rather
    /// than wrapped to a negative number.
    #[test]
    fn a_roster_uint64_past_i64_max_is_refused() {
        use arrow::array::{StringArray, UInt64Array};
        use arrow::datatypes::{Field, Schema as ArrowSchema};
        use std::sync::Arc;

        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("roster.parquet");
        let read = |rank: u64| {
            let schema = Arc::new(ArrowSchema::new(vec![
                Field::new("key", DataType::Utf8, false),
                Field::new("rank", DataType::UInt64, false),
            ]));
            let batch = RecordBatch::try_new(
                schema.clone(),
                vec![
                    Arc::new(StringArray::from(vec!["a"])),
                    Arc::new(UInt64Array::from(vec![rank])),
                ],
            )
            .unwrap();
            let mut writer =
                parquet::arrow::ArrowWriter::try_new(File::create(&path).unwrap(), schema, None)
                    .unwrap();
            writer.write(&batch).unwrap();
            writer.close().unwrap();
            read_roster_table(
                &path,
                &crate::config::Fields::canonical("roster"),
                &[crate::config::ViewMetadata {
                    name: "rank".to_string(),
                    ty: ScalarType::U64,
                    vocabulary: None,
                }],
            )
        };
        let rows = read(7).expect("a rank an i64 holds is read");
        assert!(matches!(
            rows[0].metadata["rank"],
            crate::config::MetadataValue::Int(7)
        ));
        assert!(read(1 << 63).is_err(), "a rank past i64::MAX is refused");
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

// ---------------------------------------------------------------------------------------------
// The roster as a table (`views.md` §3.1's form B)
// ---------------------------------------------------------------------------------------------

/// One row of a `[view_group.views]` table: a view of the group, as the file declares it.
///
/// **The rows are the roster, in file order**, which is the table's analogue of block order: the
/// ordinal is creation order and a build creates the views in the order it reads them.
#[derive(Debug, Clone)]
pub struct RosterRow {
    pub key: String,
    /// The view's own gate, where the table carries a `visibility` column and this row a value:
    /// one label from a string column, or the elements of a `list<string>` column, each one
    /// label (`views.md` §6). `None` takes the group's.
    pub visibility: Option<Vec<String>>,
    pub metadata: std::collections::BTreeMap<String, crate::config::MetadataValue>,
}

/// Read the roster table: one row per view, the canonical `key`, `visibility` and one column per
/// declared metadata name (`views.md` §3.1).
///
/// **Every declared name, on every row, non-null.** A roster record is immutable
/// ([decision 0108](../../../docs/decisions/0108-a-roster-record-is-immutable.md)), so a value the
/// table leaves out is a view served with that field missing for the whole of its life rather than
/// one an update fills in later — the same rule the inline block is held to.
///
/// `visibility` is the one optional column: a table carrying none is a roster of views that all
/// take the group's gate. Where it is carried it is a `string` — one label per row — or a
/// `list<string>` whose elements are the row's labels, each one term (decision 0132).
pub fn read_roster_table(
    path: &Path,
    fields: &Fields,
    metadata: &[crate::config::ViewMetadata],
) -> Result<Vec<RosterRow>> {
    use crate::config::MetadataValue;
    use arrow::array::{BooleanArray, LargeStringArray, StringArray};

    let file = File::open(path).map_err(|e| BuildError::io(path, e))?;
    let builder =
        ParquetRecordBatchReaderBuilder::try_new(file).map_err(|e| BuildError::parquet(path, e))?;
    let schema = builder.schema().clone();
    let key_name = fields.of("key").to_string();
    let visibility_name = fields.of("visibility").to_string();
    if schema.column_with_name(&key_name).is_none() {
        return Err(BuildError::Schema {
            path: path.to_path_buf(),
            detail: format!(
                "the roster table has no '{key_name}' column, and the key is the view's own name \
                 — `<group>:<key>` is the id every request names (views §3.2). Its columns are: {}",
                column_names(&schema)
            ),
        });
    }
    let carries_visibility = schema.column_with_name(&visibility_name).is_some();
    let reader = builder
        .with_batch_size(65_536)
        .build()
        .map_err(|e| BuildError::parquet(path, e))?;
    let projected = arrow::array::RecordBatchReader::schema(&reader);
    let key_idx = column_index(path, &projected, &key_name)?;
    let visibility_idx = match carries_visibility {
        true => Some(column_index(path, &projected, &visibility_name)?),
        false => None,
    };
    let metadata_idx: Vec<usize> = metadata
        .iter()
        .map(|declared| {
            let name = fields.of(&declared.name);
            column_index(path, &projected, name).map_err(|_| BuildError::Schema {
                path: path.to_path_buf(),
                detail: format!(
                    "the roster table has no '{name}' column, and this group declares '{}' as \
                     metadata every view carries (views §3.1). Its columns are: {}",
                    declared.name,
                    column_names(&projected)
                ),
            })
        })
        .collect::<Result<_>>()?;

    /// One column's rows as strings, or a refusal naming the column's type.
    fn strings(
        path: &Path,
        column: &arrow::array::ArrayRef,
        name: &str,
    ) -> Result<Vec<Option<String>>> {
        if let Some(values) = column.as_any().downcast_ref::<StringArray>() {
            return Ok((0..values.len())
                .map(|i| (!values.is_null(i)).then(|| values.value(i).to_string()))
                .collect());
        }
        if let Some(values) = column.as_any().downcast_ref::<LargeStringArray>() {
            return Ok((0..values.len())
                .map(|i| (!values.is_null(i)).then(|| values.value(i).to_string()))
                .collect());
        }
        Err(BuildError::Schema {
            path: path.to_path_buf(),
            detail: format!(
                "the roster column '{name}' has type {:?}, and this one is a string",
                column.data_type()
            ),
        })
    }

    /// The gate column's rows as label lists: a string column is one label per row, a list
    /// column is the row's labels, and a null row is no gate of its own. Both list widths and
    /// both string widths are read, as the `access` column's are (contracts §3.4 r78): the width
    /// is the writer's choice and says nothing about the labels.
    fn gates(
        path: &Path,
        column: &arrow::array::ArrayRef,
        name: &str,
    ) -> Result<Vec<Option<Vec<String>>>> {
        use arrow::array::{Array as _, LargeListArray, ListArray};
        // One row's labels out of the list's value array, `lo..hi` being its offsets.
        let row = |values: &arrow::array::ArrayRef, i: usize, lo: usize, hi: usize| {
            let label = |j: usize| -> Result<Option<String>> {
                if let Some(v) = values.as_any().downcast_ref::<StringArray>() {
                    return Ok((!v.is_null(j)).then(|| v.value(j).to_string()));
                }
                if let Some(v) = values.as_any().downcast_ref::<LargeStringArray>() {
                    return Ok((!v.is_null(j)).then(|| v.value(j).to_string()));
                }
                Err(BuildError::Schema {
                    path: path.to_path_buf(),
                    detail: format!(
                        "the roster column '{name}' is a list of {:?}, and a gate's labels are \
                         strings: `list<string>` or `large_list<string>`, of `utf8` or \
                         `large_utf8` (views §6)",
                        values.data_type()
                    ),
                })
            };
            (lo..hi)
                .map(|j| {
                    label(j)?.ok_or_else(|| BuildError::Schema {
                        path: path.to_path_buf(),
                        detail: format!(
                            "the roster column '{name}' carries a null element in row {i}, and \
                             each element of a gate is one label (views §6)"
                        ),
                    })
                })
                .collect::<Result<Vec<_>>>()
        };
        if let Some(list) = column.as_any().downcast_ref::<ListArray>() {
            let offsets = list.value_offsets();
            return (0..list.len())
                .map(|i| match list.is_null(i) {
                    true => Ok(None),
                    false => row(
                        list.values(),
                        i,
                        offsets[i] as usize,
                        offsets[i + 1] as usize,
                    )
                    .map(Some),
                })
                .collect();
        }
        if let Some(list) = column.as_any().downcast_ref::<LargeListArray>() {
            let offsets = list.value_offsets();
            return (0..list.len())
                .map(|i| match list.is_null(i) {
                    true => Ok(None),
                    false => row(
                        list.values(),
                        i,
                        offsets[i] as usize,
                        offsets[i + 1] as usize,
                    )
                    .map(Some),
                })
                .collect();
        }
        Ok(strings(path, column, name)?
            .into_iter()
            .map(|label| label.map(|l| vec![l]))
            .collect())
    }

    let mut rows: Vec<RosterRow> = Vec::new();
    for batch in reader {
        let batch = batch.map_err(|e| BuildError::arrow(path, e))?;
        let keys = strings(path, batch.column(key_idx), &key_name)?;
        let gates = match visibility_idx {
            Some(idx) => gates(path, batch.column(idx), &visibility_name)?,
            None => vec![None; keys.len()],
        };
        // One decode per column per batch, as every other reader here does: the roster is a
        // handful of rows, and the shape is the file's rather than the row's.
        let mut values: Vec<Vec<MetadataValue>> = Vec::with_capacity(metadata.len());
        for (declared, &idx) in metadata.iter().zip(&metadata_idx) {
            let column = batch.column(idx);
            let name = fields.of(&declared.name);
            let missing = |row: usize| BuildError::Schema {
                path: path.to_path_buf(),
                detail: format!(
                    "the roster's row {row} carries no '{name}', and this group declares it as \
                     metadata every view carries (views §3.1). A roster record is immutable \
                     (decision 0108), so a value left out is a view served with that field \
                     missing for the whole of its life"
                ),
            };
            let held: Vec<MetadataValue> = if declared.vocabulary.is_some() {
                // A category's key, carried as written: it is resolved against the vocabulary
                // where a category column's values are, which is not this parse's decision.
                strings(path, column, name)?
                    .into_iter()
                    .enumerate()
                    .map(|(row, value)| value.map(MetadataValue::Text).ok_or_else(|| missing(row)))
                    .collect::<Result<_>>()?
            } else {
                match declared.ty {
                    ScalarType::Bool => {
                        let values =
                            column
                                .as_any()
                                .downcast_ref::<BooleanArray>()
                                .ok_or_else(|| BuildError::Schema {
                                    path: path.to_path_buf(),
                                    detail: format!(
                                        "the roster column '{name}' has type {:?}, and this group \
                                     declares it 'bool'",
                                        column.data_type()
                                    ),
                                })?;
                        (0..values.len())
                            .map(|row| match values.is_null(row) {
                                true => Err(missing(row)),
                                false => Ok(MetadataValue::Bool(values.value(row))),
                            })
                            .collect::<Result<_>>()?
                    }
                    // Read at `f64` whatever the declared width, since a roster float is held
                    // as one.
                    ScalarType::F32 | ScalarType::F64 => {
                        let read = ScalarColumn::new(column, ScalarType::F64).ok_or_else(|| {
                            BuildError::Schema {
                                path: path.to_path_buf(),
                                detail: format!(
                                    "the roster column '{name}' has type {:?}, and this group \
                                     declares it '{}'; send float32 or float64",
                                    column.data_type(),
                                    declared.ty.arrow_type_name()
                                ),
                            }
                        })?;
                        (0..column.len())
                            .map(|row| match read.value(row) {
                                Ok(ScalarValue::F64(value)) => Ok(MetadataValue::Float(value)),
                                Ok(_) => Err(missing(row)),
                                Err(e) => unreachable!("an f64 read refuses nothing: {e}"),
                            })
                            .collect::<Result<_>>()?
                    }
                    ScalarType::Utf8 | ScalarType::Keyword | ScalarType::Text => {
                        strings(path, column, name)?
                            .into_iter()
                            .enumerate()
                            .map(|(row, value)| {
                                value.map(MetadataValue::Text).ok_or_else(|| missing(row))
                            })
                            .collect::<Result<_>>()?
                    }
                    ty => {
                        let read = ScalarColumn::new(column, ty).ok_or_else(|| {
                            BuildError::Schema {
                                path: path.to_path_buf(),
                                detail: format!(
                                    "the roster column '{name}' has type {:?}, and this group \
                                     declares it '{}'. A `timestamp_us` reads a microsecond \
                                     timestamp or an integer, and nothing else; cast a \
                                     millisecond column to microseconds first",
                                    column.data_type(),
                                    ty.arrow_type_name()
                                ),
                            }
                        })?;
                        let unfit = |row: usize, value: &dyn std::fmt::Display| BuildError::Schema {
                            path: path.to_path_buf(),
                            detail: format!(
                                "the roster column '{name}' at row {row} carries {value}; its \
                                 declared '{}' cannot hold it, so write a value that fits or \
                                 declare a wider type",
                                ty.arrow_type_name()
                            ),
                        };
                        // A roster value is carried as an `i64`, so a `u64` stops at `i64::MAX`.
                        (0..column.len())
                            .map(|row| match read.value(row) {
                                Ok(ScalarValue::Null) => Err(missing(row)),
                                Ok(ScalarValue::TimestampUs(value)) => {
                                    Ok(MetadataValue::TimestampUs(value))
                                }
                                Ok(ScalarValue::U8(value)) => Ok(MetadataValue::Int(value.into())),
                                Ok(ScalarValue::U16(value)) => Ok(MetadataValue::Int(value.into())),
                                Ok(ScalarValue::U32(value)) => Ok(MetadataValue::Int(value.into())),
                                Ok(ScalarValue::U64(value)) => i64::try_from(value)
                                    .map(MetadataValue::Int)
                                    .map_err(|_| unfit(row, &value)),
                                Ok(ScalarValue::I8(value)) => Ok(MetadataValue::Int(value.into())),
                                Ok(ScalarValue::I16(value)) => Ok(MetadataValue::Int(value.into())),
                                Ok(ScalarValue::I32(value)) => Ok(MetadataValue::Int(value.into())),
                                Ok(ScalarValue::I64(value)) => Ok(MetadataValue::Int(value)),
                                Ok(other) => {
                                    unreachable!("read as an integer or a timestamp: {other:?}")
                                }
                                Err(e) => Err(unfit(row, &e)),
                            })
                            .collect::<Result<_>>()?
                    }
                }
            };
            values.push(held);
        }
        for (row, key) in keys.into_iter().enumerate() {
            let key = key.ok_or_else(|| BuildError::Schema {
                path: path.to_path_buf(),
                detail: format!(
                    "the roster's row {row} carries no '{key_name}', and a view's key is required \
                     at creation (views §3.2)"
                ),
            })?;
            rows.push(RosterRow {
                key,
                visibility: gates[row].clone(),
                metadata: metadata
                    .iter()
                    .zip(&values)
                    .map(|(declared, held)| (declared.name.clone(), held[row].clone()))
                    .collect(),
            });
        }
    }
    Ok(rows)
}

// ---------------------------------------------------------------------------------------------
// The roster minted from the discriminator (`views.md` §3.1's third form)
// ---------------------------------------------------------------------------------------------

/// The distinct values of a group's discriminator column — the keys of a group that declares no
/// roster at all (`views.md` §3.1).
///
/// **Sorted by key bytes, because the set is a [`BTreeSet`] and not the file's order.** Roster
/// order is served order (decision 0113), so a mint that took appearance order would make the
/// order of a rebuild depend on how the source's row groups happen to be arranged — two builds of
/// one corpus serving one group's views in two orders.
///
/// A null is refused here for the reason [`selected_rows`] refuses one below: a row that names no
/// view is in no view, and the mint is the first reader to see it.
pub fn read_discriminator_keys(path: &Path, column: &str) -> Result<BTreeSet<String>> {
    use arrow::array::{LargeStringArray, StringArray};

    let file = File::open(path).map_err(|e| BuildError::io(path, e))?;
    let builder =
        ParquetRecordBatchReaderBuilder::try_new(file).map_err(|e| BuildError::parquet(path, e))?;
    let schema = builder.schema().clone();
    let root = schema
        .column_with_name(column)
        .map(|(i, _)| i)
        .ok_or_else(|| BuildError::Schema {
            path: path.to_path_buf(),
            detail: format!(
                "`fields.view = \"{column}\"` names the column whose distinct values are this \
                 group's views, and this file has no column of that name. Its columns are: {}",
                column_names(&schema)
            ),
        })?;
    let projection = parquet::arrow::ProjectionMask::roots(builder.parquet_schema(), vec![root]);
    let reader = builder
        .with_projection(projection)
        .with_batch_size(65_536)
        .build()
        .map_err(|e| BuildError::parquet(path, e))?;
    let projected = arrow::array::RecordBatchReader::schema(&reader);
    let idx = column_index(path, &projected, column)?;

    let mut keys: BTreeSet<String> = BTreeSet::new();
    for batch in reader {
        let batch = batch.map_err(|e| BuildError::arrow(path, e))?;
        let values = batch.column(idx);
        let null = || BuildError::Schema {
            path: path.to_path_buf(),
            detail: format!(
                "the discriminator column '{column}' has a null in it, and a row that names no \
                 view is in no view (views §3.1). This group's views are the distinct values of \
                 that column, so a null is neither a view nor a row of one"
            ),
        };
        if let Some(strings) = values.as_any().downcast_ref::<StringArray>() {
            for i in 0..strings.len() {
                if strings.is_null(i) {
                    return Err(null());
                }
                keys.insert(strings.value(i).to_string());
            }
            continue;
        }
        if let Some(strings) = values.as_any().downcast_ref::<LargeStringArray>() {
            for i in 0..strings.len() {
                if strings.is_null(i) {
                    return Err(null());
                }
                keys.insert(strings.value(i).to_string());
            }
            continue;
        }
        return Err(BuildError::Schema {
            path: path.to_path_buf(),
            detail: format!(
                "the discriminator column '{column}' has type {:?}, and a view key is a string \
                 (views §3.2's charset). Refused rather than coerced: a key read out of another \
                 type would mint a view under a name nobody wrote",
                values.data_type()
            ),
        });
    }
    Ok(keys)
}
