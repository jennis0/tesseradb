use std::sync::Arc;

use arrow::array::Array;
use arrow::record_batch::RecordBatch;
use tessera_engine::scalar_column::{self, ScalarColumn};
use tessera_engine::{
    DeclaredScalar, Projection, ScalarType, ScalarValue, ScopedScalar, Vocabularies,
    VocabularyKind, ABSENT_CODE,
};
use tessera_lifecycle::{BatchArtifacts, WalScalar};
use tessera_types::layer::LayerDeclaration;
use tessera_types::TesseraId;

use super::json::JsonColumns;
use super::membership::{membership_column, MembershipColumn, MembershipTally};
use super::DecodeError;
use super::{
    declared_as_scoped, scoped_absent, scoped_as_declared, scoped_wire_type, Address, BodyEncoding,
    Fixed, EXTERNAL_ID_MAX_LEN,
};

#[derive(Debug)]
pub(crate) struct RawIngestItem {
    /// `None` when the caller sent no external id; the item is then addressable only by its
    /// `tessera_id`.
    pub(crate) external_id: Option<Vec<u8>>,
    /// In the view's frame, never longitude and latitude: the projection has already run.
    pub(crate) x: f64,
    pub(crate) y: f64,
    /// The row's `access` labels, verbatim. Empty for a row with none, which the view's
    /// `point_default` fills or refuses.
    pub(crate) labels: Vec<Vec<u8>>,
    pub(crate) scalars: Vec<WalScalar>,
    /// Group-scoped values for the view's group, positional against the families the batch was
    /// parsed with; empty where the view's group owns none.
    pub(crate) scoped: Vec<WalScalar>,
}

/// A projected view's coordinate columns are `lon` and `lat`, an unprojected view's `x` and `y`,
/// as at the build. Naming the axes for what they hold catches a corpus written with the two
/// exchanged, which would otherwise be silently mirrored.
fn coordinate_columns(projection: Projection) -> (&'static str, &'static str) {
    match projection {
        Projection::None => ("x", "y"),
        _ => ("lon", "lat"),
    }
}

/// One ingest batch, decoded, with what its membership columns said.
pub(crate) struct ParsedBatch {
    pub(crate) items: Vec<RawIngestItem>,
    pub(crate) artifacts: BatchArtifacts,
    /// Declared columns the batch omitted, each absent on every row; reported so a pipeline that
    /// stopped sending one is seen.
    pub(crate) padded_columns: u64,
    /// Rows whose latitude lay outside the projection's domain and were moved onto the frame's
    /// edge; always `0` for an unprojected view.
    pub(crate) clipped: u64,
}

/// One category cell: its key resolved to the bound code, at the column's declared width. Codes
/// are resolved here and never minted: minting happens once, on the write executor, so two
/// requests racing one novel key cannot draw two codes for it.
fn category_code(
    body_name: &str,
    col: &dyn Array,
    row: usize,
    declared: &DeclaredScalar,
    vocabulary: &str,
    vocabularies: &Vocabularies,
) -> Result<WalScalar, DecodeError> {
    use arrow::array::StringArray;
    let keys = col
        .as_any()
        .downcast_ref::<StringArray>()
        .expect("a category column was validated as utf8 above");
    if keys.is_null(row) {
        return Ok(code_at(declared.arrow_type, ABSENT_CODE));
    }
    let key = keys.value(row);
    if key.is_empty() {
        return Err(DecodeError(format!(
            "{body_name}: column '{}' carries the empty string, which is not a value key. An \
             item with no value for this column carries null, which is stored as *absent*; \
             minting a code for the empty string would make a typo a category \
             (per-point-attributes §3.4)",
            declared.name
        )));
    }
    let minter = vocabularies.get(vocabulary).ok_or_else(|| {
        DecodeError(format!(
            "{body_name}: column '{}' names vocabulary '{vocabulary}', which this bundle does \
             not carry",
            declared.name
        ))
    })?;
    if let Some(code) = minter.code_of(key) {
        return Ok(code_at(declared.arrow_type, code));
    }
    match minter.kind() {
        // A declared vocabulary is closed, so an unbound key is refused here, where the whole
        // batch can still be rejected without effect.
        VocabularyKind::Declared => Err(DecodeError(format!(
            "{body_name}: column '{}' carries value '{key}', which vocabulary '{vocabulary}' \
             does not list. Under `vocabulary = \"declared\"` there is no auto-mint: a category \
             carries properties and, through its postings, a visibility consequence, so a typo \
             must not create one (per-point-attributes §5)",
            declared.name
        ))),
        // A novel key travels as a key; the executor mints its code when the commit window
        // closes, before the WAL append.
        VocabularyKind::Discovered => Ok(WalScalar::Utf8(key.to_string())),
    }
}

/// A code at its column's declared width. A category is `u8`, `u16` or `u32` wide, so the
/// fallthrough is `u32`.
pub(super) fn code_at(width: ScalarType, code: u32) -> WalScalar {
    match width {
        ScalarType::U8 => WalScalar::U8(code as u8),
        ScalarType::U16 => WalScalar::U16(code as u16),
        _ => WalScalar::U32(code),
    }
}

/// The body's record batches. A JSON body becomes one record batch, read by the same rules as
/// an Arrow one.
fn record_batches<'a>(
    body_name: &'static str,
    encoding: BodyEncoding,
    body: &'a [u8],
    json: &JsonColumns<'_>,
) -> Result<Box<dyn Iterator<Item = Result<RecordBatch, DecodeError>> + 'a>, DecodeError> {
    Ok(match encoding {
        BodyEncoding::Arrow => {
            let cursor = std::io::Cursor::new(body);
            let reader = arrow::ipc::reader::StreamReader::try_new(cursor, None).map_err(|e| {
                DecodeError(format!("{body_name} is not a valid Arrow IPC stream: {e}"))
            })?;
            Box::new(reader.map(move |batch| {
                batch.map_err(|e| DecodeError(format!("{body_name}: arrow decode error: {e}")))
            }))
        }
        BodyEncoding::Json => Box::new(std::iter::once(super::json::record_batch(
            body_name, body, json,
        ))),
    })
}

/// Checks a batch's columns before any row is read and returns its layer columns. A name is the
/// route's own, a declared scalar, a family in `scoped` or a registered layer's, matched in that
/// order, so a layer cannot redefine `x`; a declared or scoped column at the wrong type is refused.
fn check_columns<'b>(
    body_name: &str,
    batch: &'b RecordBatch,
    fixed: &[Fixed<'_>],
    declared: &[DeclaredScalar],
    scoped: &[ScopedScalar],
    layer_of: &dyn Fn(&str) -> Option<LayerDeclaration>,
    view_in: &dyn Fn(&str) -> Option<String>,
) -> Result<Vec<MembershipColumn<'b>>, DecodeError> {
    let mut declarations = Vec::new();
    for field in batch.schema_ref().fields() {
        let name = field.name().as_str();
        if fixed.iter().any(|f| f.name() == name)
            || declared.iter().any(|d| d.name == name)
            // A family under its plain name; `scoped` is empty for a view outside a group, so
            // there such a column is refused below.
            || scoped.iter().any(|f| f.name == name)
        {
            continue;
        }
        let Some(declaration) = layer_of(name) else {
            return Err(DecodeError(format!(
                "{body_name}: column '{name}' is neither in MANIFEST.declared_scalars nor the \
                 name of a registered layer, nor a group-scoped family whose key set holds this \
                 batch's view (contracts §2.2, `views.md` §5). An undeclared column is refused \
                 rather than dropped"
            )));
        };
        declarations.push((name, declaration));
    }
    let mut memberships = Vec::with_capacity(declarations.len());
    for (name, declaration) in &declarations {
        let column = batch
            .column_by_name(name)
            .expect("the column was found in this batch's own schema");
        memberships.push(membership_column(
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
        if !wire_carries(d, col.data_type()) {
            return Err(DecodeError(format!(
                "{body_name}: column '{}' is {:?}, but MANIFEST.declared_scalars declares it {} \
                 (contracts §2.6); refused rather than dropped",
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
        if !wire_carries(&scoped_as_declared(f), col.data_type()) {
            return Err(DecodeError(format!(
                "{body_name}: column '{}' is {:?}, but it is a group-scoped attribute declared {} \
                 (views §5); refused rather than dropped",
                f.name,
                col.data_type(),
                scoped_wire_type(f).arrow_type_name()
            )));
        }
    }
    Ok(memberships)
}

/// Whether a batch column of Arrow type `found` carries `declared`. A category's column is its
/// keys as `utf8`; any other is read by the rule a build reads a points file by.
fn wire_carries(declared: &DeclaredScalar, found: &arrow::datatypes::DataType) -> bool {
    match declared.vocabulary {
        Some(_) => *found == arrow::datatypes::DataType::Utf8,
        None => scalar_column::carries(declared.arrow_type, found),
    }
}

/// A column `check_columns` has passed, read once for all of one record batch's rows.
enum Cells<'b> {
    /// A category's value keys, resolved per row against its vocabulary.
    Keys(&'b dyn Array),
    Scalars(ScalarColumn),
}

impl<'b> Cells<'b> {
    fn new(col: &'b arrow::array::ArrayRef, declared: &DeclaredScalar) -> Self {
        match declared.vocabulary {
            Some(_) => Cells::Keys(col.as_ref()),
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
    cells: &Cells<'_>,
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
            category_code(body_name, *col, row, declared, vocabulary, vocabularies)
        }
        Cells::Scalars(column) => column.value(row).map(wal_scalar).map_err(|e| {
            DecodeError(format!(
                "{body_name}: row {request_row}, column '{}' (declared {}) carries {e}; send a \
                 value that fits or declare a wider type",
                declared.name,
                declared.arrow_type.arrow_type_name(),
            ))
        }),
    }
}

/// The build's value type as the write-ahead log's, variant for variant.
fn wal_scalar(value: ScalarValue) -> WalScalar {
    macro_rules! same {
        ($($v:ident),* $(,)?) => {
            match value {
                $(ScalarValue::$v(x) => WalScalar::$v(x),)*
                ScalarValue::Null => WalScalar::Null,
            }
        };
    }
    same!(Bool, U8, U16, U32, U64, I8, I16, I32, I64, F32, F64, TimestampUs, Utf8)
}

fn check_external_id(external_id: &[u8]) -> Result<(), DecodeError> {
    if external_id.len() > EXTERNAL_ID_MAX_LEN {
        return Err(DecodeError(format!(
            "external id is {} bytes, exceeding the {EXTERNAL_ID_MAX_LEN}-byte cap (contracts \
             §1); refused rather than truncated",
            external_id.len()
        )));
    }
    Ok(())
}

/// Decodes a `/control/ingest` body against the view's projection, its declared scalars, its
/// group's scoped families and the layer registry. `node_id` is accepted and not stored. Any
/// refusal refuses the whole batch.
#[allow(clippy::too_many_arguments)]
pub(crate) fn parse_ingest_batch(
    encoding: BodyEncoding,
    body: &[u8],
    projection: Projection,
    declared: &[DeclaredScalar],
    // The families of the group that owns the view, in manifest order; empty outside a group.
    scoped: &[ScopedScalar],
    vocabularies: &Vocabularies,
    layer_of: &dyn Fn(&str) -> Option<LayerDeclaration>,
    // The key of the batch's view in a group, `None` where the view is none of the group's.
    view_in: &dyn Fn(&str) -> Option<String>,
) -> Result<ParsedBatch, DecodeError> {
    let body_name = "ingest body";
    let (x_name, y_name) = coordinate_columns(projection);
    let fixed = [
        Fixed::ExternalId,
        Fixed::Coordinate(x_name),
        Fixed::Coordinate(y_name),
        Fixed::Access,
        Fixed::NodeId,
    ];
    let batches = record_batches(
        body_name,
        encoding,
        body,
        &JsonColumns {
            fixed: &fixed,
            declared_on_every_row: true,
            declared,
            scoped,
            layer_of,
        },
    )?;
    let scoped_declared: Vec<DeclaredScalar> = scoped.iter().map(scoped_as_declared).collect();
    let mut items = Vec::new();
    let mut tally = MembershipTally::default();
    let mut clipped = 0u64;
    let mut padded: std::collections::BTreeSet<&str> = std::collections::BTreeSet::new();
    for batch in batches {
        let batch = batch?;
        padded.extend(
            declared
                .iter()
                .filter(|d| batch.column_by_name(&d.name).is_none())
                .map(|d| d.name.as_str()),
        );
        // Where this record batch's rows start in the request's numbering, which a membership
        // names.
        let offset = items.len();

        let ext = optional_binary_col(body_name, &batch, "external_id")?;
        // Only where the right column is absent, so an unprojected view with a declared scalar
        // called `lon` is unaffected.
        for (wrong, right) in wrong_spellings(projection) {
            if batch.column_by_name(wrong).is_some() && batch.column_by_name(right).is_none() {
                return Err(DecodeError(format!(
                    "ingest body: {}, so its coordinate columns are '{x_name}' and '{y_name}', \
                     not '{wrong}' (projections.md §2, §3). The axes are named for what they hold \
                     because a corpus written with longitude and latitude exchanged is mirrored \
                     about the diagonal and malformed in no other way; rename '{wrong}' to \
                     '{right}'",
                    match projection {
                        Projection::None =>
                            "this view declares no projection, so it has no longitude".to_string(),
                        _ => format!("this view is projected `{}`", projection.name()),
                    }
                )));
            }
        }
        let mut x = coordinate_col(&batch, x_name, offset)?;
        let mut y = coordinate_col(&batch, y_name, offset)?;
        // The projection runs here, before anything else reads the coordinates, as the build
        // runs it on a points file.
        clipped += project_columns(projection, &mut x, &mut y)?;
        let access = labels_col(&batch, "access")?;

        let memberships =
            check_columns(body_name, &batch, &fixed, declared, scoped, layer_of, view_in)?;
        let declared_cells: Vec<Option<Cells<'_>>> = declared
            .iter()
            .map(|d| batch.column_by_name(&d.name).map(|col| Cells::new(col, d)))
            .collect();
        let scoped_cells: Vec<Option<Cells<'_>>> = scoped_declared
            .iter()
            .map(|d| batch.column_by_name(&d.name).map(|col| Cells::new(col, d)))
            .collect();

        for i in 0..batch.num_rows() {
            for column in &memberships {
                tally.read(body_name, column, i, offset)?;
            }
            // In declared order, not schema order: the vector is read back by position. A
            // declared column the batch omits keeps its slot, absent, so an older client's batch
            // misaligns nothing.
            let mut scalars = Vec::with_capacity(declared.len());
            for (d, cells) in declared.iter().zip(&declared_cells) {
                let Some(cells) = cells else {
                    scalars.push(scoped_absent(&declared_as_scoped(d)));
                    continue;
                };
                scalars.push(cell(body_name, cells, i, offset + i, d, vocabularies)?);
            }
            // The scoped values are a second positional list, in the families' order, since the
            // two lists are indexed against different declarations.
            let mut scoped_values = Vec::with_capacity(scoped.len());
            let families = scoped.iter().zip(&scoped_declared).zip(&scoped_cells);
            for ((f, as_declared), cells) in families {
                let value = match cells {
                    None => scoped_absent(f),
                    Some(cells) => {
                        cell(body_name, cells, i, offset + i, as_declared, vocabularies)?
                    }
                };
                scoped_values.push(value);
            }
            // A missing column and a null cell both mean the item has no external id.
            let external_id = match &ext {
                Some(arr) if !arr.is_null(i) => Some(arr.value(i).to_vec()),
                _ => None,
            };
            // Checked inside the parse, so an over-length id anywhere refuses the whole batch
            // before deduplication, allocation or the WAL append.
            if let Some(external_id) = &external_id {
                check_external_id(external_id)?;
            }
            items.push(RawIngestItem {
                external_id,
                x: x[i],
                y: y[i],
                labels: access.labels_at(i)?,
                scalars,
                scoped: scoped_values,
            });
        }
    }
    Ok(ParsedBatch {
        items,
        artifacts: tally.into_artifacts(),
        padded_columns: padded.len() as u64,
        clipped,
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
    let fixed = [Fixed::ExternalId, Fixed::TesseraId, Fixed::IdSet];
    let batches = record_batches(
        body_name,
        encoding,
        body,
        &JsonColumns {
            fixed: &fixed,
            // A values row may leave a column out, which leaves that cell unfilled.
            declared_on_every_row: false,
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
        let batch = batch?;
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
                "values body: two record batches of one stream carry different columns; a batch \
                 is one column set, so every row's values are positional against one list"
                    .to_string(),
            ));
        }

        let memberships =
            check_columns(body_name, &batch, &fixed, declared, scoped, layer_of, view_in)?;

        let ext = optional_binary_col(body_name, &batch, "external_id")?;
        let tessera = match batch.column_by_name("tessera_id") {
            None => None,
            Some(col) => Some(
                col.as_any()
                    .downcast_ref::<arrow::array::StringArray>()
                    .ok_or_else(|| {
                        DecodeError(
                            "values body: column 'tessera_id' is present but not utf8; a \
                             tessera_id is decimal digits in a string"
                                .to_string(),
                        )
                    })?,
            ),
        };
        let idset = match batch.column_by_name("idset") {
            None => None,
            Some(col) => Some(
                col.as_any()
                    .downcast_ref::<arrow::array::UInt32Array>()
                    .ok_or_else(|| {
                        DecodeError(
                            "values body: column 'idset' is present but not uint32".to_string(),
                        )
                    })?,
            ),
        };

        let cells: Vec<Cells<'_>> = carried
            .iter()
            .map(|d| {
                let col = batch
                    .column_by_name(&d.name)
                    .expect("`carried` holds only the batch's own columns");
                Cells::new(col, d)
            })
            .collect();

        for i in 0..batch.num_rows() {
            for column in &memberships {
                tally.read(body_name, column, i, offset)?;
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
                        "values body: row {} names both an external_id and a tessera_id; a row \
                         names its entity exactly one way",
                        rows.len()
                    )))
                }
                (None, None) => {
                    return Err(DecodeError(format!(
                        "values body: row {} names no entity; a values row carries an \
                         external_id, or a tessera_id with its idset (`ingest.md` §1.4)",
                        rows.len()
                    )))
                }
                (Some(external), None) => {
                    check_external_id(&external)?;
                    Address::External(external)
                }
                (None, Some(named)) => {
                    let id = named.parse::<u64>().map_err(|_| {
                        DecodeError(format!(
                            "values body: row {}'s tessera_id is not decimal digits",
                            rows.len()
                        ))
                    })?;
                    // An idset is required beside a `tessera_id`, as on `/control/changes`: an
                    // id names an entity only under the set it was minted in.
                    let Some(set) = idset.as_ref().filter(|arr| !arr.is_null(i)) else {
                        return Err(DecodeError(format!(
                            "values body: row {} names a tessera_id with no idset; the \
                             identifier set is required beside one, from `/v1/meta`",
                            rows.len()
                        )));
                    };
                    Address::Tessera {
                        id: TesseraId::new(id),
                        idset: set.value(i),
                    }
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

/// The coordinate columns a batch for this view must not carry, each paired with its right name.
fn wrong_spellings(projection: Projection) -> [(&'static str, &'static str); 2] {
    match projection {
        Projection::None => [("lon", "x"), ("lat", "y")],
        _ => [("x", "lon"), ("y", "lat")],
    }
}

/// Projects the coordinate columns in place, returning how many rows were clipped. A point
/// outside WGS84's range is refused, as at the build; one outside only the projection's domain
/// is moved onto the frame's edge and counted, never refused.
fn project_columns(
    projection: Projection,
    x: &mut [f64],
    y: &mut [f64],
) -> Result<u64, DecodeError> {
    if projection == Projection::None {
        return Ok(0);
    }
    let mut clipped = 0u64;
    for (row, (lon, lat)) in x.iter_mut().zip(y.iter_mut()).enumerate() {
        if !lon.is_finite() || !lat.is_finite() || lon.abs() > 180.0 || lat.abs() > 90.0 {
            return Err(DecodeError(format!(
                "ingest body: row {row} is at lon {lon}, lat {lat}, which is not a place. This \
                 view is projected ({}), and the accepted input coordinate system is WGS84 \
                 degrees — longitude within ±180, latitude within ±90 (projections.md §2). The \
                 whole batch is refused, so nothing was queued or appended",
                projection.name()
            )));
        }
        // Counted here because the engine's out-of-frame check reads the frame's edge as inside,
        // so at the whole-world frame it never sees a clipped row.
        clipped += u64::from(projection.is_clipped(*lat));
        let (px, py) = projection.forward(*lon, *lat);
        (*lon, *lat) = (px, py);
    }
    Ok(clipped)
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

/// A coordinate column as `f64`: `float32` is widened, as the build reads a points file, and
/// `float64` is never narrowed, since at deep zoom narrowing would move a point to another cell.
/// `offset` is where this record batch's rows start in the request, so a null names its row.
fn coordinate_col(
    batch: &arrow::record_batch::RecordBatch,
    name: &str,
    offset: usize,
) -> Result<Vec<f64>, DecodeError> {
    let column = batch.column_by_name(name).ok_or_else(|| {
        DecodeError(format!(
            "ingest body: column '{name}' missing or not float32/float64"
        ))
    })?;
    if let Some(row) = (0..column.len()).find(|&row| column.is_null(row)) {
        return Err(DecodeError(format!(
            "ingest body: row {}, column '{name}' is null; write a number, since every row needs \
             both coordinates",
            offset + row
        )));
    }
    let any = column.as_any();
    if let Some(a) = any.downcast_ref::<arrow::array::Float64Array>() {
        Ok(a.values().to_vec())
    } else if let Some(a) = any.downcast_ref::<arrow::array::Float32Array>() {
        Ok(a.values().iter().map(|v| f64::from(*v)).collect())
    } else {
        Err(DecodeError(format!(
            "ingest body: column '{name}' missing or not float32/float64"
        )))
    }
}

/// The `access` column, `list<utf8>` or `large_list<utf8>`: each element is one label, taken
/// verbatim, so a label containing a separator is one term, as at the build. A scalar `utf8`
/// column is refused rather than read as one label per row.
enum LabelCells<'a> {
    List(&'a arrow::array::ListArray),
    Large(&'a arrow::array::LargeListArray),
}

impl LabelCells<'_> {
    fn values(&self) -> &arrow::array::StringArray {
        let values: &Arc<dyn Array> = match self {
            LabelCells::List(list) => list.values(),
            LabelCells::Large(list) => list.values(),
        };
        values
            .as_any()
            .downcast_ref::<arrow::array::StringArray>()
            .expect("labels_col checked the element type")
    }

    /// The range of `values()` row `row` occupies, or `None` where the row's list is null.
    fn entries(&self, row: usize) -> Option<std::ops::Range<usize>> {
        match self {
            LabelCells::List(list) => (!list.is_null(row)).then(|| {
                let offsets = list.value_offsets();
                offsets[row] as usize..offsets[row + 1] as usize
            }),
            LabelCells::Large(list) => (!list.is_null(row)).then(|| {
                let offsets = list.value_offsets();
                offsets[row] as usize..offsets[row + 1] as usize
            }),
        }
    }

    /// Row `row`'s labels, verbatim and in order. A null or empty list is a row with no label,
    /// which the view's declared default fills or refuses, as the JSON decode reads a null
    /// `access`; a null element is refused.
    fn labels_at(&self, row: usize) -> Result<Vec<Vec<u8>>, DecodeError> {
        let Some(entries) = self.entries(row) else {
            return Ok(Vec::new());
        };
        let values = self.values();
        entries
            .map(|index| {
                if values.is_null(index) {
                    return Err(DecodeError(format!(
                        "ingest body: column 'access' has a null element at row {row}; every \
                         element of a row's list is one label, taken verbatim"
                    )));
                }
                Ok(values.value(index).as_bytes().to_vec())
            })
            .collect()
    }
}

fn labels_col<'a>(
    batch: &'a arrow::record_batch::RecordBatch,
    name: &str,
) -> Result<LabelCells<'a>, DecodeError> {
    use arrow::array::{LargeListArray, ListArray};
    use arrow::datatypes::DataType;

    const SHAPE: &str = "list<utf8> or large_list<utf8>, one label per element, an empty list \
                         for a row with no label (contracts §3.4)";
    let Some(column) = batch.column_by_name(name) else {
        return Err(DecodeError(format!(
            "ingest body: column '{name}' missing; it is {SHAPE}"
        )));
    };
    let cells = match column.data_type() {
        DataType::List(_) => LabelCells::List(
            column
                .as_any()
                .downcast_ref::<ListArray>()
                .expect("a List column downcasts to a ListArray"),
        ),
        DataType::LargeList(_) => LabelCells::Large(
            column
                .as_any()
                .downcast_ref::<LargeListArray>()
                .expect("a LargeList column downcasts to a LargeListArray"),
        ),
        DataType::Utf8 | DataType::LargeUtf8 => {
            return Err(DecodeError(format!(
                "ingest body: column '{name}' is utf8, one string per row; it is {SHAPE}. Each \
                 element is one label, verbatim, so a label containing a comma is one term \
                 (decision 0129)"
            )));
        }
        other => {
            return Err(DecodeError(format!(
                "ingest body: column '{name}' is {other:?}; it is {SHAPE}"
            )));
        }
    };
    let element = match &cells {
        LabelCells::List(list) => list.values().data_type().clone(),
        LabelCells::Large(list) => list.values().data_type().clone(),
    };
    if element != DataType::Utf8 {
        return Err(DecodeError(format!(
            "ingest body: column '{name}' is a list of {element:?}; it is {SHAPE}"
        )));
    }
    Ok(cells)
}

#[cfg(test)]
mod category_wire {
    use super::*;
    use arrow::array::{Float32Array, StringArray, UInt8Array};
    use arrow::datatypes::{DataType, Field, Schema};
    use arrow::record_batch::RecordBatch;
    use std::sync::Arc;
    use tessera_engine::{DeclaredScalar, Vocabularies};

    const CODE_OPS: u32 = 4711;

    fn declared() -> Vec<DeclaredScalar> {
        vec![
            DeclaredScalar {
                name: "department".to_string(),
                arrow_type: ScalarType::U16,
                vocabulary: Some("departments".to_string()),
                analyser: None,
                index: false,
                render: true,
            },
            DeclaredScalar {
                name: "score".to_string(),
                arrow_type: ScalarType::F32,
                vocabulary: None,
                analyser: None,
                index: false,
                render: true,
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
            Projection::None,
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
            Projection::None,
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
            Projection::None,
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
