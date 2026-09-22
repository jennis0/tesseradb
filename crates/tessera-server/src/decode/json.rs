//! `/control/ingest`'s JSON encoding (ingest §1.2): an array of objects or newline-delimited
//! objects, one object per row, coerced against the declared column types into one Arrow
//! `RecordBatch`.
//!
//! **The batch is then decoded by the Arrow path and by nothing else.** Every rule the Arrow decode
//! applies per column (the scalar tail against the manifest, the label list, the membership
//! column's arity, the projection) runs on what this module built, so a batch sent as JSON and the
//! same batch sent as Arrow reach the executor as one row form. What this module owns is the
//! coercion of one JSON value into one Arrow cell, and its refusals name the row and the column.
//!
//! An integer is parsed exactly from its digits, as a JSON number or as a string of digits, never
//! through a double: a 64-bit identifier or value survives the door. A timestamp is microseconds
//! since the epoch as an integer, the spelling the view roster's `timestamp_us` already takes. A
//! float column takes any JSON number and narrows to the declared width. An external id is base64,
//! as every external id on this plane is. A null is absence for every column that has one; a null
//! or absent `access` is a row with no label, which the view's declared default fills or refuses
//! (decision 0133), and a null element of it is refused.

use std::sync::Arc;

use arrow::array::{
    Array, ArrayRef, BinaryBuilder, BooleanBuilder, Float32Builder, Float64Builder, Int16Builder,
    Int32Builder, Int64Builder, Int8Builder, ListBuilder, StringBuilder,
    TimestampMicrosecondBuilder, UInt16Builder, UInt32Builder, UInt64Builder, UInt8Builder,
};
use arrow::datatypes::{DataType, Field, Schema};
use arrow::record_batch::RecordBatch;
use base64::Engine as _;
use serde_json::{Map, Value};
use tessera_engine::{DeclaredScalar, ScalarType, ScopedScalar};
use tessera_types::layer::LayerDeclaration;

use crate::error::ApiError;

/// What the batch's columns may be, resolved once per batch from the manifest and the layer
/// registry by the caller, in the same order the Arrow decode resolves them.
pub(crate) struct JsonColumns<'a> {
    pub x_name: &'a str,
    pub y_name: &'a str,
    pub declared: &'a [DeclaredScalar],
    pub scoped: &'a [ScopedScalar],
    pub layer_of: &'a dyn Fn(&str) -> Option<LayerDeclaration>,
}

/// One JSON body as one record batch. The first three columns are the coordinate pair and
/// `access`; `external_id` and `node_id` are present where any row carries them; then the
/// declared scalars in declared order, the scoped families any row names, and the layer columns
/// in first-appearance order.
pub(crate) fn record_batch(
    body: &[u8],
    columns: &JsonColumns<'_>,
) -> Result<RecordBatch, ApiError> {
    let body_name = "ingest body";
    let rows = records(body_name, body)?;
    let mut fields: Vec<Field> = Vec::new();
    let mut arrays: Vec<ArrayRef> = Vec::new();

    let has = |name: &str| rows.iter().any(|row| row.contains_key(name));

    if has("external_id") {
        let mut builder = BinaryBuilder::new();
        for (row, record) in rows.iter().enumerate() {
            match record.get("external_id") {
                None | Some(Value::Null) => builder.append_null(),
                Some(Value::String(text)) => {
                    let bytes = base64::engine::general_purpose::STANDARD
                        .decode(text)
                        .map_err(|_| {
                            refusal(
                                body_name,
                                row,
                                "external_id",
                                "is not base64; an external id is bytes",
                            )
                        })?;
                    builder.append_value(bytes);
                }
                Some(_) => {
                    return Err(refusal(
                        body_name,
                        row,
                        "external_id",
                        "is not a string; an external id is base64",
                    ))
                }
            }
        }
        fields.push(Field::new("external_id", DataType::Binary, true));
        arrays.push(Arc::new(builder.finish()));
    }

    for name in [columns.x_name, columns.y_name] {
        let mut builder = Float64Builder::new();
        for (row, record) in rows.iter().enumerate() {
            match record.get(name) {
                Some(Value::Number(number)) => builder.append_value(
                    number
                        .as_f64()
                        .ok_or_else(|| refusal(body_name, row, name, "is not a finite number"))?,
                ),
                None | Some(Value::Null) => {
                    return Err(refusal(
                        body_name,
                        row,
                        name,
                        "is missing or null; a coordinate is required",
                    ))
                }
                Some(_) => return Err(refusal(body_name, row, name, "is not a number")),
            }
        }
        fields.push(Field::new(name, DataType::Float64, false));
        arrays.push(Arc::new(builder.finish()));
    }

    {
        let mut builder = ListBuilder::new(StringBuilder::new());
        for (row, record) in rows.iter().enumerate() {
            match record.get("access") {
                // A row with no label (ingest §1.2, decision 0133): the view's declaration decides.
                None | Some(Value::Null) => builder.append(true),
                Some(Value::Array(labels)) => {
                    for label in labels {
                        match label {
                            Value::String(text) => builder.values().append_value(text),
                            _ => {
                                return Err(refusal(
                                    body_name,
                                    row,
                                    "access",
                                    "has an element that is not a string; every element is one \
                                     label, taken verbatim",
                                ))
                            }
                        }
                    }
                    builder.append(true);
                }
                Some(_) => {
                    return Err(refusal(
                        body_name,
                        row,
                        "access",
                        "is not a list of labels, one label per element (contracts §3.4)",
                    ))
                }
            }
        }
        let array = builder.finish();
        fields.push(Field::new("access", array.data_type().clone(), false));
        arrays.push(Arc::new(array));
    }

    if has("node_id") {
        let mut builder = StringBuilder::new();
        for (row, record) in rows.iter().enumerate() {
            match record.get("node_id") {
                None | Some(Value::Null) => builder.append_null(),
                Some(Value::String(text)) => builder.append_value(text),
                Some(_) => return Err(refusal(body_name, row, "node_id", "is not a string")),
            }
        }
        fields.push(Field::new("node_id", DataType::Utf8, true));
        arrays.push(Arc::new(builder.finish()));
    }

    // **A declared column no row names is omitted from the batch**, which the Arrow decode reads
    // as the column padded with its absence in every row and reports on the receipt
    // (`ingest.md` §7.1). A column some rows name is carried by every row of the batch, null
    // where a row has no value, and a row omitting it is a `422` naming the row and the column
    // (contracts §3.4). The two doors then agree about what a batch that stopped carrying a
    // column looks like, and about what a half-carried column is.
    for declared in columns.declared {
        if !has(&declared.name) {
            continue;
        }
        let column = scalar_column(body_name, &rows, &declared.name, declared.wire_type(), true)?;
        fields.push(Field::new(&declared.name, column.data_type().clone(), true));
        arrays.push(column);
    }
    for family in columns.scoped {
        if !has(&family.name) {
            continue;
        }
        let wire = super::scoped_wire_type(family);
        let column = scalar_column(body_name, &rows, &family.name, wire, false)?;
        fields.push(Field::new(&family.name, column.data_type().clone(), true));
        arrays.push(column);
    }

    // Every other name is a layer's or is refused, on the Arrow decode's rule.
    let known = |name: &str| {
        matches!(name, "external_id" | "access" | "node_id")
            || name == columns.x_name
            || name == columns.y_name
            || columns.declared.iter().any(|d| d.name == name)
            || columns.scoped.iter().any(|f| f.name == name)
    };
    let mut layers: Vec<String> = Vec::new();
    for (row, record) in rows.iter().enumerate() {
        for name in record.keys() {
            if known(name) || layers.iter().any(|l| l == name) {
                continue;
            }
            if (columns.layer_of)(name).is_none() {
                return Err(ApiError::Contract(format!(
                    "ingest body: row {row}, column '{name}' is neither in \
                     MANIFEST.declared_scalars nor the name of a registered layer (contracts \
                     §2.2). Scalars are stored positionally against the declared order, so an \
                     undeclared column is refused rather than dropped"
                )));
            }
            layers.push(name.clone());
        }
    }
    for name in &layers {
        let column = membership_column(body_name, &rows, name)?;
        fields.push(Field::new(name, column.data_type().clone(), true));
        arrays.push(column);
    }

    RecordBatch::try_new(Arc::new(Schema::new(fields)), arrays)
        .map_err(|e| ApiError::Contract(format!("ingest body: {e}")))
}

/// One `POST /control/values` body as one record batch (`ingest.md` §1.2, §1.4).
///
/// **The same coercion as the ingest door, over a different column set.** A values row addresses
/// an entity that exists rather than creating one, so it carries no coordinates and no `access`
/// list, and exactly one of `external_id` and `tessera_id`; everything after that is a declared
/// column, a group-scoped family this batch's view may name, or a layer's, and every cell is
/// coerced by the same functions the ingest door's cells are.
pub(crate) fn values_record_batch(
    body: &[u8],
    columns: &JsonColumns<'_>,
) -> Result<RecordBatch, ApiError> {
    let body_name = "values body";
    let rows = records(body_name, body)?;
    let mut fields: Vec<Field> = Vec::new();
    let mut arrays: Vec<ArrayRef> = Vec::new();
    let has = |name: &str| rows.iter().any(|row| row.contains_key(name));

    if has("external_id") {
        let mut builder = BinaryBuilder::new();
        for (row, record) in rows.iter().enumerate() {
            match record.get("external_id") {
                None | Some(Value::Null) => builder.append_null(),
                Some(Value::String(text)) => {
                    let bytes = base64::engine::general_purpose::STANDARD
                        .decode(text)
                        .map_err(|_| {
                            refusal(
                                body_name,
                                row,
                                "external_id",
                                "is not base64; an external id is bytes",
                            )
                        })?;
                    builder.append_value(bytes);
                }
                Some(_) => {
                    return Err(refusal(
                        body_name,
                        row,
                        "external_id",
                        "is not a string; an external id is base64",
                    ))
                }
            }
        }
        fields.push(Field::new("external_id", DataType::Binary, true));
        arrays.push(Arc::new(builder.finish()));
    }
    // **String-encoded, as it is on `/control/changes`**: a bare JSON number loses a `u64` past
    // 2⁵³ in every JavaScript client, and a mis-parsed identifier fills the wrong entity.
    if has("tessera_id") {
        let mut builder = StringBuilder::new();
        for (row, record) in rows.iter().enumerate() {
            match record.get("tessera_id") {
                None | Some(Value::Null) => builder.append_null(),
                Some(Value::String(text)) => builder.append_value(text),
                Some(_) => {
                    return Err(refusal(
                        body_name,
                        row,
                        "tessera_id",
                        "is not a string; a tessera_id is decimal digits in a string, so that a \
                         64-bit identifier survives a JavaScript client",
                    ))
                }
            }
        }
        fields.push(Field::new("tessera_id", DataType::Utf8, true));
        arrays.push(Arc::new(builder.finish()));
    }
    if has("idset") {
        let mut builder = UInt32Builder::new();
        for (row, record) in rows.iter().enumerate() {
            match record.get("idset") {
                None | Some(Value::Null) => builder.append_null(),
                other => match integer(body_name, other.unwrap_or(&Value::Null), row, "idset")? {
                    None => builder.append_null(),
                    Some(value) => builder.append_value(u32::try_from(value).map_err(|_| {
                        refusal(
                            body_name,
                            row,
                            "idset",
                            "is out of range for an identifier set",
                        )
                    })?),
                },
            }
        }
        fields.push(Field::new("idset", DataType::UInt32, true));
        arrays.push(Arc::new(builder.finish()));
    }

    for declared in columns.declared {
        if !has(&declared.name) {
            continue;
        }
        // **Not `required`**: a values batch carries the subset of the schema the caller has, and
        // a row of it may leave a column out, which is that cell unfilled rather than a malformed
        // row. The ingest door's stricter reading is about a row that creates an entity, where a
        // half-carried column shifts the positional tail.
        let column = scalar_column(
            body_name,
            &rows,
            &declared.name,
            declared.wire_type(),
            false,
        )?;
        fields.push(Field::new(&declared.name, column.data_type().clone(), true));
        arrays.push(column);
    }
    for family in columns.scoped {
        if !has(&family.name) {
            continue;
        }
        let wire = super::scoped_wire_type(family);
        let column = scalar_column(body_name, &rows, &family.name, wire, false)?;
        fields.push(Field::new(&family.name, column.data_type().clone(), true));
        arrays.push(column);
    }

    let known = |name: &str| {
        matches!(name, "external_id" | "tessera_id" | "idset")
            || columns.declared.iter().any(|d| d.name == name)
            || columns.scoped.iter().any(|f| f.name == name)
    };
    let mut layers: Vec<String> = Vec::new();
    for (row, record) in rows.iter().enumerate() {
        for name in record.keys() {
            if known(name) || layers.iter().any(|l| l == name) {
                continue;
            }
            if (columns.layer_of)(name).is_none() {
                return Err(ApiError::Contract(format!(
                    "values body: row {row}, column '{name}' is neither in \
                     MANIFEST.declared_scalars, nor a group-scoped family whose key set holds \
                     this batch's view, nor the name of a registered layer (contracts §2.2, \
                     `views.md` §5). An undeclared column is refused rather than dropped"
                )));
            }
            layers.push(name.clone());
        }
    }
    for name in &layers {
        let column = membership_column(body_name, &rows, name)?;
        fields.push(Field::new(name, column.data_type().clone(), true));
        arrays.push(column);
    }

    RecordBatch::try_new(Arc::new(Schema::new(fields)), arrays)
        .map_err(|e| ApiError::Contract(format!("values body: {e}")))
}

/// The body's records: a JSON array of objects, or one object per line.
fn records(body_name: &str, body: &[u8]) -> Result<Vec<Map<String, Value>>, ApiError> {
    let text = std::str::from_utf8(body)
        .map_err(|_| ApiError::Contract(format!("{body_name} is not UTF-8 JSON")))?;
    let trimmed = text.trim_start();
    if trimmed.starts_with('[') {
        let values: Vec<Value> = serde_json::from_str(trimmed).map_err(|e| {
            ApiError::Contract(format!("{body_name} is not a JSON array of objects: {e}"))
        })?;
        return values
            .into_iter()
            .enumerate()
            .map(|(row, value)| match value {
                Value::Object(record) => Ok(record),
                _ => Err(ApiError::Contract(format!(
                    "{body_name}: row {row} is not an object; each record is one object whose \
                     names are the column names"
                ))),
            })
            .collect();
    }
    let mut rows = Vec::new();
    for line in text.lines() {
        if line.trim().is_empty() {
            continue;
        }
        let row = rows.len();
        match serde_json::from_str::<Value>(line) {
            Ok(Value::Object(record)) => rows.push(record),
            Ok(_) => {
                return Err(ApiError::Contract(format!(
                    "{body_name}: row {row} is not an object; each line is one object whose \
                     names are the column names"
                )))
            }
            Err(e) => {
                return Err(ApiError::Contract(format!(
                    "{body_name}: row {row} is not JSON: {e}. The body is an array of objects \
                     or one object per line"
                )))
            }
        }
    }
    Ok(rows)
}

fn refusal(body_name: &str, row: usize, column: &str, what: &str) -> ApiError {
    ApiError::Contract(format!("{body_name}: row {row}, column '{column}' {what}"))
}

/// One scalar column at its wire type. A declared scalar is `required`: a batch that carries the
/// name at all carries it on every row, null for absence, as every Arrow batch carries the
/// column on every row (contracts §3.4). A batch no row names it in does not reach here, the
/// caller having omitted the column (`ingest.md` §7.1). A scoped family's column is absent on the
/// rows that omit it.
fn scalar_column(
    body_name: &str,
    rows: &[Map<String, Value>],
    name: &str,
    wire: ScalarType,
    required: bool,
) -> Result<ArrayRef, ApiError> {
    let cell = |row: usize| -> Result<&Value, ApiError> {
        match rows[row].get(name) {
            Some(value) => Ok(value),
            None if required => Err(refusal(
                body_name,
                row,
                name,
                "is missing; a declared column a batch carries is on every row of it, null where \
                 the row has no value (contracts §3.4)",
            )),
            None => Ok(&Value::Null),
        }
    };
    macro_rules! integers {
        ($builder:ty, $ty:ty, $spelling:literal) => {{
            let mut builder = <$builder>::new();
            for row in 0..rows.len() {
                match integer(body_name, cell(row)?, row, name)? {
                    None => builder.append_null(),
                    Some(value) => builder.append_value(<$ty>::try_from(value).map_err(|_| {
                        refusal(
                            body_name,
                            row,
                            name,
                            concat!("is out of range for ", $spelling),
                        )
                    })?),
                }
            }
            Arc::new(builder.finish()) as ArrayRef
        }};
    }
    Ok(match wire {
        ScalarType::Bool => {
            let mut builder = BooleanBuilder::new();
            for row in 0..rows.len() {
                match cell(row)? {
                    Value::Null => builder.append_null(),
                    Value::Bool(value) => builder.append_value(*value),
                    _ => return Err(refusal(body_name, row, name, "is not a boolean")),
                }
            }
            Arc::new(builder.finish())
        }
        ScalarType::U8 => integers!(UInt8Builder, u8, "u8"),
        ScalarType::U16 => integers!(UInt16Builder, u16, "u16"),
        ScalarType::U32 => integers!(UInt32Builder, u32, "u32"),
        ScalarType::U64 => integers!(UInt64Builder, u64, "u64"),
        ScalarType::I8 => integers!(Int8Builder, i8, "i8"),
        ScalarType::I16 => integers!(Int16Builder, i16, "i16"),
        ScalarType::I32 => integers!(Int32Builder, i32, "i32"),
        ScalarType::I64 => integers!(Int64Builder, i64, "i64"),
        ScalarType::TimestampUs => {
            let mut builder = TimestampMicrosecondBuilder::new();
            for row in 0..rows.len() {
                match integer(body_name, cell(row)?, row, name)? {
                    None => builder.append_null(),
                    Some(value) => builder.append_value(i64::try_from(value).map_err(|_| {
                        refusal(body_name, row, name, "is out of range for timestamp_us")
                    })?),
                }
            }
            Arc::new(builder.finish())
        }
        ScalarType::F32 => {
            let mut builder = Float32Builder::new();
            for row in 0..rows.len() {
                match float(body_name, cell(row)?, row, name)? {
                    None => builder.append_null(),
                    // Narrowed to the declared width, and refused where the narrowing would
                    // store an infinity for a finite number.
                    Some(value) => {
                        let narrowed = value as f32;
                        if !narrowed.is_finite() {
                            return Err(refusal(
                                body_name,
                                row,
                                name,
                                "is outside f32's finite range",
                            ));
                        }
                        builder.append_value(narrowed);
                    }
                }
            }
            Arc::new(builder.finish())
        }
        ScalarType::F64 => {
            let mut builder = Float64Builder::new();
            for row in 0..rows.len() {
                match float(body_name, cell(row)?, row, name)? {
                    None => builder.append_null(),
                    Some(value) => builder.append_value(value),
                }
            }
            Arc::new(builder.finish())
        }
        ScalarType::Utf8 | ScalarType::Keyword | ScalarType::Text => {
            let mut builder = StringBuilder::new();
            for row in 0..rows.len() {
                match cell(row)? {
                    Value::Null => builder.append_null(),
                    Value::String(text) => builder.append_value(text),
                    _ => return Err(refusal(body_name, row, name, "is not a string")),
                }
            }
            Arc::new(builder.finish())
        }
    })
}

/// An integer, exactly: a JSON integer or a string of digits; a number with a fraction or
/// exponent is refused rather than rounded.
fn integer(
    body_name: &str,
    value: &Value,
    row: usize,
    name: &str,
) -> Result<Option<i128>, ApiError> {
    match value {
        Value::Null => Ok(None),
        Value::Number(number) => {
            if let Some(v) = number.as_u64() {
                Ok(Some(v as i128))
            } else if let Some(v) = number.as_i64() {
                Ok(Some(v as i128))
            } else {
                Err(refusal(
                    body_name,
                    row,
                    name,
                    "has a fraction or an exponent; an integer is parsed exactly from its digits",
                ))
            }
        }
        Value::String(text) => text.parse::<i128>().map(Some).map_err(|_| {
            refusal(
                body_name,
                row,
                name,
                "is a string that is not an integer; an integer is a JSON integer or a string of \
                 digits",
            )
        }),
        _ => Err(refusal(body_name, row, name, "is not an integer")),
    }
}

fn float(body_name: &str, value: &Value, row: usize, name: &str) -> Result<Option<f64>, ApiError> {
    match value {
        Value::Null => Ok(None),
        Value::Number(number) => number
            .as_f64()
            .map(Some)
            .ok_or_else(|| refusal(body_name, row, name, "is not a finite number")),
        _ => Err(refusal(body_name, row, name, "is not a number")),
    }
}

/// A column named for a layer: a key, or a list of keys, per row, at the values a member table
/// carries (contracts §3.4). An integer key is its decimal spelling and `-1` names no artifact,
/// through the same `integer_key` the Arrow decode and the build read by, so the cell reaches the
/// decoder as the key it names. The column is one shape: every row a key or null, or every row
/// a list or null; a row of the other shape is refused, since a scalar names the artifact at
/// level 0 and a list's positions mean what the layer's hierarchy says, and guessing which the
/// caller meant would store a membership they did not write.
fn membership_column(
    body_name: &str,
    rows: &[Map<String, Value>],
    name: &str,
) -> Result<ArrayRef, ApiError> {
    let is_list = rows
        .iter()
        .any(|record| matches!(record.get(name), Some(Value::Array(_))));
    let key = |value: &Value, row: usize| -> Result<Option<String>, ApiError> {
        match value {
            Value::Null => Ok(None),
            Value::String(text) => Ok(Some(text.clone())),
            Value::Number(_) => {
                Ok(integer(body_name, value, row, name)?
                    .and_then(tessera_types::layer::integer_key))
            }
            _ => Err(refusal(
                body_name,
                row,
                name,
                "is not a member key; a key is text or an integer, and `null` or `-1` is a point \
                 in no artifact of that layer",
            )),
        }
    };
    if is_list {
        let mut builder = ListBuilder::new(StringBuilder::new());
        for (row, record) in rows.iter().enumerate() {
            match record.get(name) {
                None | Some(Value::Null) => builder.append_null(),
                Some(Value::Array(entries)) => {
                    for entry in entries {
                        match key(entry, row)? {
                            Some(text) => builder.values().append_value(text),
                            None => builder.values().append_null(),
                        }
                    }
                    builder.append(true);
                }
                Some(_) => {
                    return Err(refusal(
                        body_name,
                        row,
                        name,
                        "is a single key where other rows of this column carry a list; a column \
                         is one shape",
                    ))
                }
            }
        }
        return Ok(Arc::new(builder.finish()));
    }
    let mut builder = StringBuilder::new();
    for (row, record) in rows.iter().enumerate() {
        match record.get(name) {
            None => builder.append_null(),
            Some(value) => match key(value, row)? {
                Some(text) => builder.append_value(text),
                None => builder.append_null(),
            },
        }
    }
    Ok(Arc::new(builder.finish()))
}
