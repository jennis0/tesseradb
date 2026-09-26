use arrow::array::{Array, UInt32Array};
use arrow::record_batch::RecordBatch;
use tessera_engine::coordinates;
use tessera_engine::member_key;
use tessera_engine::shapes::Bounds;
use tessera_engine::scalar_column::{self, ScalarColumn};
use tessera_engine::utf8::{Utf8Column, Utf8Values};
use tessera_engine::vocabulary::{code_value, Resolved};
use tessera_engine::{absent_scalar, DeclaredScalar, Projection, ScopedScalar, Vocabularies};
use tessera_lifecycle::{BatchArtifacts, WalScalar};
use tessera_types::layer::LayerDeclaration;
use tessera_types::TesseraId;

use super::json::JsonColumns;
use super::membership::{membership_column, MembershipColumn, MembershipTally};
use super::DecodeError;
use super::{
    scoped_as_declared, scoped_wire_type, Address, BodyEncoding, Fixed, EXTERNAL_ID_MAX_LEN,
};

#[derive(Debug)]
pub(crate) struct RawIngestItem {
    /// `None` when the caller sent no external id.
    pub(crate) external_id: Option<Vec<u8>>,
    pub(crate) tessera_id: Option<TesseraId>,
    /// In the view's frame, never longitude and latitude: the projection has already run. `None`
    /// for a row carrying no coordinates.
    pub(crate) position: Option<(f64, f64)>,
    /// The row's `access` labels, trimmed; `None` where the row leaves its label out, and empty
    /// where it sends no label, which the view's default decides.
    pub(crate) labels: Option<Vec<Vec<u8>>>,
    pub(crate) scalars: Vec<WalScalar>,
    /// Group-scoped values for the view's group, positional against the families the batch was
    /// parsed with; empty where the view's group owns none.
    pub(crate) scoped: Vec<WalScalar>,
    /// The columns this row left out, as positions in the declared scalars followed by the scoped
    /// families; each holds its absence in `scalars` or `scoped`.
    pub(crate) omitted: Vec<usize>,
}

/// One ingest batch, decoded, with what its membership columns said.
pub(crate) struct ParsedBatch {
    pub(crate) items: Vec<RawIngestItem>,
    pub(crate) artifacts: BatchArtifacts,
    /// Rows whose latitude lay outside the projection's domain and were moved onto the frame's
    /// edge; always `0` for an unprojected view.
    pub(crate) clipped: u64,
    /// Rows outside the view's extent, moved onto its edge.
    pub(crate) clamped: u64,
}

/// The frame an ingest batch's coordinates are read against: its view's projection and extent.
pub(crate) struct Frame<'a> {
    pub(crate) projection: Projection,
    pub(crate) extent: &'a Bounds,
}

/// One category cell: its key resolved to the bound code, at the column's declared width. Codes
/// are resolved here and never minted: minting happens once, on the write executor, so two
/// requests racing one novel key cannot draw two codes for it.
fn category_code(
    body_name: &str,
    keys: Utf8Column<'_>,
    row: usize,
    declared: &DeclaredScalar,
    vocabulary: &str,
    vocabularies: &Vocabularies,
) -> Result<WalScalar, DecodeError> {
    let minter = vocabularies.get(vocabulary).ok_or_else(|| {
        DecodeError(format!(
            "{body_name}: column '{}' names vocabulary '{vocabulary}', which this bundle does \
             not carry",
            declared.name
        ))
    })?;
    match minter.resolve(keys.at(row)) {
        Ok(Resolved::Code(code)) => Ok(code_value(declared.arrow_type, code)),
        // The executor mints a novel key's code at a commit window's close or in a values batch's
        // pass, before the WAL append.
        Ok(Resolved::Novel(key)) => Ok(WalScalar::Utf8(key.to_string())),
        Err(e) => Err(DecodeError(format!(
            "{body_name}: column '{}' {e}",
            declared.name
        ))),
    }
}

/// Which of the columns in [`JsonColumns::required`] each row of a record batch leaves out, as
/// positions in that list: one set for every row of an Arrow batch, which carries a column or does
/// not, and one per record of a JSON body.
enum Omitted {
    Batch(Vec<usize>),
    Rows(Vec<Vec<usize>>),
}

impl Omitted {
    fn of(&self, row: usize) -> &[usize] {
        match self {
            Omitted::Batch(columns) => columns,
            Omitted::Rows(rows) => &rows[row],
        }
    }
}

type Batches<'a> = Box<dyn Iterator<Item = Result<(RecordBatch, Omitted), DecodeError>> + 'a>;

/// The body's record batches, each with the columns its rows leave out. A JSON body becomes one
/// record batch, read by the same rules as an Arrow one.
fn record_batches<'a>(
    body_name: &'static str,
    encoding: BodyEncoding,
    body: &'a [u8],
    json: &JsonColumns<'_>,
) -> Result<Batches<'a>, DecodeError> {
    Ok(match encoding {
        BodyEncoding::Arrow => {
            let cursor = std::io::Cursor::new(body);
            let reader = arrow::ipc::reader::StreamReader::try_new(cursor, None).map_err(|e| {
                DecodeError(format!("{body_name} is not a valid Arrow IPC stream: {e}"))
            })?;
            let required: Vec<String> = json.required.iter().map(|name| name.to_string()).collect();
            Box::new(reader.map(move |batch| {
                let batch = batch
                    .map_err(|e| DecodeError(format!("{body_name}: arrow decode error: {e}")))?;
                let absent = (0..required.len())
                    .filter(|&at| batch.column_by_name(&required[at]).is_none())
                    .collect();
                Ok((batch, Omitted::Batch(absent)))
            }))
        }
        BodyEncoding::Json => Box::new(std::iter::once(
            super::json::record_batch(body_name, body, json)
                .map(|(batch, rows)| (batch, Omitted::Rows(rows))),
        )),
    })
}

/// A batch's layer columns, and the `level` column that places their scalar keys.
struct Memberships<'b> {
    columns: Vec<MembershipColumn<'b>>,
    levels: Option<&'b UInt32Array>,
}

/// Checks a batch's columns before any row is read and returns its layer columns. A name is the
/// route's own, a declared scalar, a family in `scoped`, `level` or a registered layer's, matched
/// in that order, so a layer cannot redefine `x`; a declared or scoped column at the wrong type is
/// refused. A `level` column places the batch's member keys, and is refused without a layer column.
fn check_columns<'b>(
    body_name: &str,
    batch: &'b RecordBatch,
    fixed: &[Fixed<'_>],
    declared: &[DeclaredScalar],
    scoped: &[ScopedScalar],
    layer_of: &dyn Fn(&str) -> Option<LayerDeclaration>,
    view_in: &dyn Fn(&str) -> Option<String>,
) -> Result<Memberships<'b>, DecodeError> {
    let undeclared = |name: &str| {
        DecodeError(format!(
            "{body_name}: column '{name}' is not a declared scalar, a registered layer or a \
             group-scoped attribute of this batch's view; declare it or leave it out"
        ))
    };
    let mut declarations = Vec::new();
    for field in batch.schema_ref().fields() {
        let name = field.name().as_str();
        if fixed.iter().any(|f| f.name() == name)
            || declared.iter().any(|d| d.name == name)
            // A family under its plain name; `scoped` is empty for a view outside a group, so
            // there such a column is refused below.
            || scoped.iter().any(|f| f.name == name)
            || name == member_key::LEVEL
        {
            continue;
        }
        match layer_of(name) {
            Some(declaration) => declarations.push((name, declaration)),
            None => return Err(undeclared(name)),
        }
    }
    let levels = match batch.column_by_name(member_key::LEVEL) {
        None => None,
        Some(_) if declarations.is_empty() => return Err(undeclared(member_key::LEVEL)),
        Some(column) => Some(member_key::read_levels(column.as_ref()).ok_or_else(|| {
            DecodeError(format!(
                "{body_name}: column '{}' places the batch's member keys at a level and is \
                 {:?}; send it as uint32",
                member_key::LEVEL,
                column.data_type()
            ))
        })?),
    };
    let mut columns = Vec::with_capacity(declarations.len());
    for (name, declaration) in &declarations {
        let column = batch
            .column_by_name(name)
            .expect("the column was found in this batch's own schema");
        columns.push(membership_column(
            body_name,
            name,
            column,
            declaration,
            view_in,
        )?);
    }
    for d in declared {
        let Some(col) = batch.column_by_name(&d.name) else {
            continue;
        };
        if !scalar_column::carries(d.arrow_type, d.vocabulary.is_some(), col.data_type()) {
            return Err(DecodeError(format!(
                "{body_name}: column '{}' is {:?} but is declared {}; send it as that type",
                d.name,
                col.data_type(),
                d.wire_type().arrow_type_name()
            )));
        }
    }
    // Scoped families' columns are type-checked on the declared columns' rule.
    for f in scoped {
        let Some(col) = batch.column_by_name(&f.name) else {
            continue;
        };
        if !scalar_column::carries(f.arrow_type, f.vocabulary.is_some(), col.data_type()) {
            return Err(DecodeError(format!(
                "{body_name}: column '{}' is {:?} but is a group-scoped attribute declared {}; \
                 send it as that type",
                f.name,
                col.data_type(),
                scoped_wire_type(f).arrow_type_name()
            )));
        }
    }
    Ok(Memberships { columns, levels })
}

/// A column `check_columns` has passed, read once for all of one record batch's rows.
enum Cells {
    /// A category's value keys, resolved per row against its vocabulary.
    Keys(Utf8Values),
    Scalars(ScalarColumn),
}

impl Cells {
    fn new(col: &arrow::array::ArrayRef, declared: &DeclaredScalar) -> Self {
        match declared.vocabulary {
            Some(_) => Cells::Keys(
                scalar_column::category_keys(col).expect("check_columns checked the column's type"),
            ),
            None => Cells::Scalars(
                ScalarColumn::new(col, declared.arrow_type)
                    .expect("check_columns checked the column's type"),
            ),
        }
    }
}

/// One row's value of a column, read against `declared`. `request_row` is the row's number in
/// the request, for a refusal to name.
fn cell(
    body_name: &str,
    cells: &Cells,
    row: usize,
    request_row: usize,
    declared: &DeclaredScalar,
    vocabularies: &Vocabularies,
) -> Result<WalScalar, DecodeError> {
    match cells {
        Cells::Keys(col) => {
            let vocabulary = declared
                .vocabulary
                .as_deref()
                .expect("a column of keys is a category's");
            category_code(body_name, col.column(), row, declared, vocabulary, vocabularies)
        }
        Cells::Scalars(column) => column.value(row).map_err(|e| {
            DecodeError(format!(
                "{body_name}: row {request_row}, column '{}' (declared {}) carries {e}; send a \
                 value that fits or declare a wider type",
                declared.name,
                declared.arrow_type.arrow_type_name(),
            ))
        }),
    }
}

fn check_external_id(external_id: &[u8]) -> Result<(), DecodeError> {
    if external_id.len() > EXTERNAL_ID_MAX_LEN {
        return Err(DecodeError(format!(
            "external id is {} bytes, over the {EXTERNAL_ID_MAX_LEN}-byte limit; send at most \
             that many",
            external_id.len()
        )));
    }
    Ok(())
}

/// Decodes a `/control/ingest` body against the view's frame, its declared scalars, its group's
/// scoped families and the layer registry. A batch naming no view has no frame and carries no
/// coordinates. `node_id` is accepted and not stored. Any refusal refuses the whole batch.
#[allow(clippy::too_many_arguments)]
pub(crate) fn parse_ingest_batch(
    encoding: BodyEncoding,
    body: &[u8],
    frame: Option<Frame<'_>>,
    declared: &[DeclaredScalar],
    // The families of the group that owns the view, in manifest order; empty outside a group.
    scoped: &[ScopedScalar],
    vocabularies: &Vocabularies,
    layer_of: &dyn Fn(&str) -> Option<LayerDeclaration>,
    // The key of the batch's view in a group, `None` where the view is none of the group's.
    view_in: &dyn Fn(&str) -> Option<String>,
) -> Result<ParsedBatch, DecodeError> {
    let body_name = "ingest body";
    let (x_name, y_name) = match &frame {
        Some(frame) => coordinates::axis_names(frame.projection),
        None => ("", ""),
    };
    let mut fixed = vec![
        Fixed::ExternalId,
        Fixed::TesseraId,
        Fixed::Access,
        Fixed::NodeId,
    ];
    if frame.is_some() {
        fixed.extend([Fixed::Coordinate(x_name), Fixed::Coordinate(y_name)]);
    }
    let columns: Vec<&str> = declared
        .iter()
        .map(|d| d.name.as_str())
        .chain(scoped.iter().map(|f| f.name.as_str()))
        .collect();
    let batches = record_batches(
        body_name,
        encoding,
        body,
        &JsonColumns {
            fixed: &fixed,
            required: &columns,
            declared,
            scoped,
            layer_of,
        },
    )?;
    let scoped_declared: Vec<DeclaredScalar> = scoped.iter().map(scoped_as_declared).collect();
    let mut items = Vec::new();
    let mut tally = MembershipTally::default();
    let (mut clipped, mut clamped) = (0u64, 0u64);
    for batch in batches {
        let (batch, omitted) = batch?;
        // Where this record batch's rows start in the request's numbering, which a membership
        // names.
        let offset = items.len();

        let ext = optional_binary_col(body_name, &batch, "external_id")?;
        let tessera = tessera_id_col(body_name, &batch)?;
        let has_column = |name: &str| batch.column_by_name(name).is_some();
        let (x, y) = match &frame {
            None => {
                if let Some(name) = ["x", "y", "longitude", "latitude"]
                    .into_iter()
                    .find(|name| has_column(name))
                {
                    return Err(DecodeError(format!(
                        "ingest body: column '{name}' is a coordinate and this batch names no \
                         view to place it in; name the view in x-tessera-view"
                    )));
                }
                (None, None)
            }
            Some(frame) => {
                if let Some((wrong, right)) = coordinates::misnamed_axis(frame.projection, has_column)
                {
                    return Err(DecodeError(format!(
                        "ingest body: {}, so its coordinate columns are '{x_name}' and \
                         '{y_name}'; rename '{wrong}' to '{right}'",
                        match frame.projection {
                            Projection::None =>
                                "this view declares no projection, so it has no longitude"
                                    .to_string(),
                            _ => format!("this view is projected `{}`", frame.projection.name()),
                        }
                    )));
                }
                (
                    coordinate_col(&batch, x_name)?,
                    coordinate_col(&batch, y_name)?,
                )
            }
        };
        if x.is_some() != y.is_some() {
            let (carried, missing) = if x.is_some() {
                (x_name, y_name)
            } else {
                (y_name, x_name)
            };
            return Err(DecodeError(format!(
                "ingest body: the batch carries '{carried}' and not '{missing}'; send both \
                 coordinates or neither"
            )));
        }
        let access_column = batch.column_by_name("access");
        let access = labels_col(body_name, &batch, "access")?;

        let memberships =
            check_columns(body_name, &batch, &fixed, declared, scoped, layer_of, view_in)?;
        // A column the batch leaves out holds its absence, which is what a null cell reads as.
        let cells_of =
            |d: &DeclaredScalar| batch.column_by_name(&d.name).map(|col| Cells::new(col, d));
        let declared_cells: Vec<Option<Cells>> = declared.iter().map(cells_of).collect();
        let scoped_cells: Vec<Option<Cells>> = scoped_declared.iter().map(cells_of).collect();
        let value_of = |cells: &Option<Cells>, i: usize, d: &DeclaredScalar| match cells {
            Some(cells) => cell(body_name, cells, i, offset + i, d, vocabularies),
            None => Ok(absent_scalar(d)),
        };

        for i in 0..batch.num_rows() {
            let position = match (x.as_ref().map(|x| x[i]), y.as_ref().map(|y| y[i])) {
                (Some(Some(x)), Some(Some(y))) => {
                    let frame = frame.as_ref().expect("coordinates are read against a frame");
                    let placed =
                        coordinates::place(frame.projection, Some(frame.extent), x, y).map_err(
                            |e| DecodeError(format!("ingest body: row {} {e}", offset + i)),
                        )?;
                    clipped += u64::from(placed.clipped);
                    clamped += u64::from(placed.clamped());
                    Some((placed.x, placed.y))
                }
                (None | Some(None), None | Some(None)) => None,
                _ => {
                    return Err(DecodeError(format!(
                        "ingest body: row {} carries one coordinate and not the other; send both \
                         or neither",
                        offset + i
                    )))
                }
            };
            for column in &memberships.columns {
                tally.read(body_name, column, memberships.levels, i, offset)?;
            }
            // In declared order, not schema order: the vector is read back by position.
            let mut scalars = Vec::with_capacity(declared.len());
            for (d, cells) in declared.iter().zip(&declared_cells) {
                scalars.push(value_of(cells, i, d)?);
            }
            // The scoped values are a second positional list, in the families' order, since the
            // two lists are indexed against different declarations.
            let mut scoped_values = Vec::with_capacity(scoped.len());
            for (d, cells) in scoped_declared.iter().zip(&scoped_cells) {
                scoped_values.push(value_of(cells, i, d)?);
            }
            let external_id = match &ext {
                Some(arr) if !arr.is_null(i) => Some(arr.value(i).to_vec()),
                _ => None,
            };
            // Checked inside the parse, so an over-length id anywhere refuses the whole batch.
            if let Some(external_id) = &external_id {
                check_external_id(external_id)?;
            }
            let tessera_id = match &tessera {
                Some(arr) if !arr.is_null(i) => {
                    Some(parse_tessera_id(body_name, offset + i, arr.value(i))?)
                }
                _ => None,
            };
            let labels = match (&access, access_column) {
                (Some(access), Some(column)) if !column.is_null(i) => Some(
                    access
                        .labels(i)
                        .map(|label| label.as_bytes().to_vec())
                        .collect(),
                ),
                _ => None,
            };
            items.push(RawIngestItem {
                external_id,
                tessera_id,
                position,
                labels,
                scalars,
                scoped: scoped_values,
                omitted: omitted.of(i).to_vec(),
            });
        }
    }
    Ok(ParsedBatch {
        items,
        artifacts: tally.into_artifacts(),
        clipped,
        clamped,
    })
}

/// The batch's `tessera_id` column, decimal digits in a string, or `None` where it has none.
fn tessera_id_col<'a>(
    body_name: &str,
    batch: &'a RecordBatch,
) -> Result<Option<&'a arrow::array::StringArray>, DecodeError> {
    match batch.column_by_name("tessera_id") {
        None => Ok(None),
        Some(col) => col
            .as_any()
            .downcast_ref::<arrow::array::StringArray>()
            .map(Some)
            .ok_or_else(|| {
                DecodeError(format!(
                    "{body_name}: column 'tessera_id' is not utf8; send each tessera_id as \
                     decimal digits in a string"
                ))
            }),
    }
}

fn parse_tessera_id(body_name: &str, row: usize, text: &str) -> Result<TesseraId, DecodeError> {
    text.parse::<u64>().map(TesseraId::new).map_err(|_| {
        DecodeError(format!(
            "{body_name}: row {row}'s tessera_id is not decimal digits; send it as a string of \
             digits"
        ))
    })
}

/// One decoded `POST /control/values` batch, before its addresses are resolved.
pub(crate) struct ParsedValues {
    /// The declared and group-scoped column names this batch carries, in the order every row's
    /// values are positional against.
    pub(crate) columns: Vec<String>,
    pub(crate) rows: Vec<ParsedValuesRow>,
    pub(crate) artifacts: BatchArtifacts,
}

/// One values row: how it names its entity, and its cells.
pub(crate) struct ParsedValuesRow {
    pub(crate) address: Address,
    pub(crate) values: Vec<WalScalar>,
}

/// Decodes a `/control/values` body into named columns and rows. A row carries no coordinates
/// or `access` and names its entity by exactly one of `external_id` and `tessera_id`. A scoped
/// column is refused unless `scoped`, which the caller resolves from the view header, holds it.
pub(crate) fn parse_values_batch(
    encoding: BodyEncoding,
    body: &[u8],
    declared: &[DeclaredScalar],
    scoped: &[ScopedScalar],
    vocabularies: &Vocabularies,
    layer_of: &dyn Fn(&str) -> Option<LayerDeclaration>,
    // As on [`parse_ingest_batch`]; a batch that names no view is in no group.
    view_in: &dyn Fn(&str) -> Option<String>,
) -> Result<ParsedValues, DecodeError> {
    let body_name = "values body";
    let fixed = [Fixed::ExternalId, Fixed::TesseraId];
    let batches = record_batches(
        body_name,
        encoding,
        body,
        &JsonColumns {
            fixed: &fixed,
            // A values row may leave a column out, which leaves that cell unfilled.
            required: &[],
            declared,
            scoped,
            layer_of,
        },
    )?;
    let scoped_declared: Vec<DeclaredScalar> = scoped.iter().map(scoped_as_declared).collect();

    let mut rows: Vec<ParsedValuesRow> = Vec::new();
    let mut columns: Vec<String> = Vec::new();
    let mut tally = MembershipTally::default();
    for batch in batches {
        let (batch, _) = batch?;
        let offset = rows.len();

        // The batch's cells, in one order for every row: declared columns, then scoped families.
        let carried: Vec<&DeclaredScalar> = declared
            .iter()
            .chain(&scoped_declared)
            .filter(|d| batch.column_by_name(&d.name).is_some())
            .collect();
        let names: Vec<String> = carried.iter().map(|d| d.name.clone()).collect();
        if rows.is_empty() {
            columns = names;
        } else if columns != names {
            return Err(DecodeError(
                "values body: two record batches of one stream carry different columns; send \
                 the same columns in every batch"
                    .to_string(),
            ));
        }

        let memberships =
            check_columns(body_name, &batch, &fixed, declared, scoped, layer_of, view_in)?;

        let ext = optional_binary_col(body_name, &batch, "external_id")?;
        let tessera = tessera_id_col(body_name, &batch)?;

        let cells: Vec<Cells> = carried
            .iter()
            .map(|d| {
                let col = batch
                    .column_by_name(&d.name)
                    .expect("`carried` holds only the batch's own columns");
                Cells::new(col, d)
            })
            .collect();

        for i in 0..batch.num_rows() {
            for column in &memberships.columns {
                tally.read(body_name, column, memberships.levels, i, offset)?;
            }
            let external = match &ext {
                Some(arr) if !arr.is_null(i) => Some(arr.value(i).to_vec()),
                _ => None,
            };
            let named = match &tessera {
                Some(arr) if !arr.is_null(i) => Some(arr.value(i)),
                _ => None,
            };
            let address = match (external, named) {
                (Some(_), Some(_)) => {
                    return Err(DecodeError(format!(
                        "values body: row {} names both an external_id and a tessera_id; send \
                         one of them",
                        rows.len()
                    )))
                }
                (None, None) => {
                    return Err(DecodeError(format!(
                        "values body: row {} names no entity; send an external_id or a \
                         tessera_id",
                        rows.len()
                    )))
                }
                (Some(external), None) => {
                    check_external_id(&external)?;
                    Address::External(external)
                }
                (None, Some(named)) => {
                    Address::Tessera(parse_tessera_id(body_name, rows.len(), named)?)
                }
            };

            let mut values = Vec::with_capacity(carried.len());
            for (d, cells) in carried.iter().zip(&cells) {
                values.push(cell(body_name, cells, i, offset + i, d, vocabularies)?);
            }
            rows.push(ParsedValuesRow { address, values });
        }
    }
    Ok(ParsedValues {
        columns,
        rows,
        artifacts: tally.into_artifacts(),
    })
}

/// An optional binary column: `None` when absent, refused when present at another type.
fn optional_binary_col<'a>(
    body_name: &str,
    batch: &'a arrow::record_batch::RecordBatch,
    name: &str,
) -> Result<Option<&'a arrow::array::BinaryArray>, DecodeError> {
    match batch.column_by_name(name) {
        None => Ok(None),
        Some(col) => col
            .as_any()
            .downcast_ref::<arrow::array::BinaryArray>()
            .map(Some)
            .ok_or_else(|| {
                DecodeError(format!(
                    "{body_name}: column '{name}' present but not binary"
                ))
            }),
    }
}

/// A coordinate column, read by the rule a build reads a points file's, with `None` for a row
/// carrying no position; `None` for a batch without the column.
fn coordinate_col(
    batch: &arrow::record_batch::RecordBatch,
    name: &str,
) -> Result<Option<Vec<Option<f64>>>, DecodeError> {
    let Some(column) = batch.column_by_name(name) else {
        return Ok(None);
    };
    coordinates::read_optional_coordinates(column.as_ref())
        .map(Some)
        .map_err(|e| DecodeError(format!("ingest body: column '{name}' {e}")))
}

/// The batch's labels column `name`, read by the rule the build reads a points file's access
/// column by, or `None` where the batch does not carry it.
pub(crate) fn labels_col<'a>(
    body_name: &str,
    batch: &'a arrow::record_batch::RecordBatch,
    name: &str,
) -> Result<Option<tessera_engine::access_column::AccessBatch<'a>>, DecodeError> {
    let Some(column) = batch.column_by_name(name) else {
        return Ok(None);
    };
    tessera_engine::access_column::read_access_column(column, name)
        .map(Some)
        .map_err(|detail| DecodeError(format!("{body_name}: {detail}")))
}

#[cfg(test)]
mod category_wire {
    use super::*;
    use arrow::array::{Float32Array, StringArray, UInt8Array};
    use arrow::datatypes::{DataType, Field, Schema};
    use arrow::record_batch::RecordBatch;
    use std::sync::Arc;
    use tessera_engine::{DeclaredScalar, ScalarType, Vocabularies, VocabularyKind, ABSENT_CODE};

    const CODE_OPS: u32 = 4711;
    const EXTENT: Bounds = Bounds {
        x_min: 0.0,
        x_max: 1.0,
        y_min: 0.0,
        y_max: 1.0,
    };

    fn declared() -> Vec<DeclaredScalar> {
        vec![
            DeclaredScalar {
                name: "department".to_string(),
                arrow_type: ScalarType::U16,
                vocabulary: Some("departments".to_string()),
                analyser: None,
                index: false,
                render: true,
                unique: false,
            },
            DeclaredScalar {
                name: "score".to_string(),
                arrow_type: ScalarType::F32,
                vocabulary: None,
                analyser: None,
                index: false,
                render: true,
                unique: false,
            },
        ]
    }

    fn vocabularies() -> Vocabularies {
        vocabularies_of(VocabularyKind::Declared)
    }

    fn vocabularies_of(kind: VocabularyKind) -> Vocabularies {
        Vocabularies::seed(
            &[tessera_engine::ManifestVocabulary {
                name: "departments".to_string(),
                kind,
                visibility: tessera_engine::Visibility::Derived,
                width: tessera_engine::ScalarType::U16,
                values: vec![tessera_engine::ManifestVocabularyValue {
                    key: "ops".to_string(),
                    code: CODE_OPS,
                    title: None,
                }],
                reserved: Vec::new(),
            }],
            &declared(),
            &[],
        )
        .expect("the fixture bundle is consistent")
    }

    /// One batch of the fixed columns plus `department` (as `column`) and `score`.
    fn body(column: arrow::array::ArrayRef, nullable: bool) -> Vec<u8> {
        let mut access = arrow::array::ListBuilder::new(arrow::array::StringBuilder::new());
        access.values().append_value("public");
        access.append(true);
        let access = access.finish();
        let schema = Arc::new(Schema::new(vec![
            Field::new("x", DataType::Float32, false),
            Field::new("y", DataType::Float32, false),
            Field::new("access", access.data_type().clone(), false),
            Field::new("department", column.data_type().clone(), nullable),
            Field::new("score", DataType::Float32, false),
        ]));
        let batch = RecordBatch::try_new(
            Arc::clone(&schema),
            vec![
                Arc::new(Float32Array::from(vec![0.5])),
                Arc::new(Float32Array::from(vec![0.5])),
                Arc::new(access),
                column,
                Arc::new(Float32Array::from(vec![1.0])),
            ],
        )
        .expect("the fixture batch is well-formed");
        let mut out = Vec::new();
        {
            let mut w = arrow::ipc::writer::StreamWriter::try_new(&mut out, &schema).unwrap();
            w.write(&batch).unwrap();
            w.finish().unwrap();
        }
        out
    }

    fn parse(
        column: arrow::array::ArrayRef,
        nullable: bool,
    ) -> Result<Vec<RawIngestItem>, DecodeError> {
        parse_ingest_batch(
            BodyEncoding::Arrow,
            &body(column, nullable),
            Some(Frame {
                projection: Projection::None,
                extent: &EXTENT,
            }),
            &declared(),
            &[],
            &vocabularies(),
            &no_layers,
            &|_| None,
        )
        .map(|parsed| parsed.items)
    }

    /// No layer is registered, so every column here is a scalar or a refusal.
    fn no_layers(_: &str) -> Option<tessera_types::layer::LayerDeclaration> {
        None
    }

    #[test]
    fn a_known_key_is_stored_as_its_pinned_code() {
        let items = parse(Arc::new(StringArray::from(vec!["ops"])), false)
            .expect("a declared key is accepted");
        assert_eq!(
            items[0].scalars[0],
            WalScalar::U16(CODE_OPS as u16),
            "the row carries the vocabulary's code, at the declared width"
        );
    }

    #[test]
    fn an_unknown_key_is_refused_naming_the_column_and_the_key() {
        let err = parse(Arc::new(StringArray::from(vec!["k9-unit"])), false)
            .expect_err("an undeclared key is refused");
        let DecodeError(detail) = err;
        assert!(detail.contains("department"), "{detail}");
        assert!(detail.contains("k9-unit"), "{detail}");
    }

    /// A category is `utf8` on the wire, so a code sent in place of its key is refused.
    #[test]
    fn a_code_on_the_wire_is_refused_where_it_used_to_be_stored() {
        let err = parse(Arc::new(UInt8Array::from(vec![9u8])), false)
            .expect_err("a category is utf8 on the wire, whatever stores its codes");
        let DecodeError(detail) = err;
        assert!(detail.contains("department"), "{detail}");
        assert!(detail.contains("utf8"), "{detail}");
    }

    #[test]
    fn a_null_key_is_absent() {
        let items = parse(
            Arc::new(StringArray::from(vec![None as Option<&str>])),
            true,
        )
        .expect("an item may carry no value for a column");
        assert_eq!(items[0].scalars[0], WalScalar::U16(ABSENT_CODE as u16));
    }

    /// An unset field and a client bug both produce the empty string, so it is not absence.
    #[test]
    fn the_empty_string_is_refused_rather_than_folded_into_absence() {
        let err = parse(Arc::new(StringArray::from(vec![""])), false)
            .expect_err("the empty string is not a value key");
        let DecodeError(detail) = err;
        assert!(detail.contains("department"), "{detail}");
    }

    /// A novel key reaches the write executor as a key, for it to mint.
    #[test]
    fn a_novel_key_under_a_discovered_vocabulary_travels_unresolved() {
        let items = parse_ingest_batch(
            BodyEncoding::Arrow,
            &body(Arc::new(StringArray::from(vec!["k9-unit"])), false),
            Some(Frame {
                projection: Projection::None,
                extent: &EXTENT,
            }),
            &declared(),
            &[],
            &vocabularies_of(VocabularyKind::Discovered),
            &no_layers,
            &|_| None,
        )
        .expect("a discovered vocabulary accepts a key it has not seen")
        .items;
        assert_eq!(
            items[0].scalars[0],
            WalScalar::Utf8("k9-unit".to_string()),
            "the key reaches the executor as a key; a code here would be a handler that mints"
        );
    }

    /// Only a novel key waits for the executor.
    #[test]
    fn a_bound_key_under_a_discovered_vocabulary_still_resolves_here() {
        let items = parse_ingest_batch(
            BodyEncoding::Arrow,
            &body(Arc::new(StringArray::from(vec!["ops"])), false),
            Some(Frame {
                projection: Projection::None,
                extent: &EXTENT,
            }),
            &declared(),
            &[],
            &vocabularies_of(VocabularyKind::Discovered),
            &no_layers,
            &|_| None,
        )
        .expect("a bound key is bound whatever the kind")
        .items;
        assert_eq!(items[0].scalars[0], WalScalar::U16(CODE_OPS as u16));
    }

    /// A plain scalar beside a category is read at its own type.
    #[test]
    fn a_plain_scalar_is_unaffected_by_the_category_rule() {
        let items = parse(Arc::new(StringArray::from(vec!["ops"])), false).unwrap();
        assert_eq!(items[0].scalars[1], WalScalar::F32(1.0));
    }
}
