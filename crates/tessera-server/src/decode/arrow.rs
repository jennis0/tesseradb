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
    /// Optional (contracts §3.4): `None` when the caller supplied no external id. Such an item
    /// gets no sidecar entry and is addressable only by its `tessera_id` (returned per row in
    /// [`IngestResp`]).
    pub(crate) external_id: Option<Vec<u8>>,
    /// **The view's frame, never longitude and latitude** — the transform has already run
    /// (`projections.md` §3). Everything downstream of the decode reads a frame coordinate: the
    /// engine's out-of-frame check, the WAL record, the buffer and the flush's quantiser.
    pub(crate) x: f64,
    pub(crate) y: f64,
    /// The row's labels, one element of the wire's `access` list each, verbatim (decision 0129).
    /// Empty for a row that carries none, which the view's `point_default` then fills or refuses
    /// (decision 0133); a null list or a null element is refused at the parse.
    pub(crate) labels: Vec<Vec<u8>>,
    pub(crate) scalars: Vec<WalScalar>,
    /// The group-scoped values this row carries for its view's group, positional against the
    /// families the batch was parsed with (`views.md` §5). Empty for a plain view and for a group
    /// that owns no family.
    pub(crate) scoped: Vec<WalScalar>,
}

/// What this view's coordinate columns are called, and what the wrong spelling would have meant.
///
/// **A projected view spells them `lon` and `lat`; a view with no projection spells them `x` and
/// `y`** (`projections.md` §2). Longitude-then-latitude is the order GeoJSON and WKT use and the
/// opposite of the order many sources publish, and a corpus written with the two exchanged is
/// silently mirrored about the diagonal — so the axes are named for what they hold rather than
/// documented. The build applies the same rule to a points file's columns
/// (`tessera_build::config`'s `compile_projected_fields`), and it has to exist on both paths or a
/// projected view is something that can be built correctly and ingested into wrongly
/// (decision 0091).
fn coordinate_columns(projection: Projection) -> (&'static str, &'static str) {
    match projection {
        Projection::None => ("x", "y"),
        _ => ("lon", "lat"),
    }
}

/// One ingest batch, decoded: its rows, what its membership columns said, and how many of those
/// rows the view's projection clipped.
pub(crate) struct ParsedBatch {
    pub(crate) items: Vec<RawIngestItem>,
    pub(crate) artifacts: BatchArtifacts,
    /// How many declared columns the batch omitted, each padded with its absence in every row
    /// (`ingest.md` §7.1). Reported so a pipeline that stopped sending a column is seen.
    pub(crate) padded_columns: u64,
    /// Rows whose latitude fell outside the projection's own domain and were moved onto the
    /// frame's edge (`projections.md` §7). Always `0` under `projection = "none"`, which has no
    /// domain.
    pub(crate) clipped: u64,
}

/// One category cell: its value key resolved to the pinned code, at the column's declared width.
///
/// **Resolution, never minting.** A handler that minted would let two requests racing one novel key
/// draw two codes for it, splitting its rows between them, and whichever binding survived would
/// recolour the other's. Minting happens once, on the write executor, where windows close serially
/// (write-path §1.1).
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
        // Declare-then-use: the value set is closed, so a key nothing binds is a typo — and a
        // category carries properties and, through its postings, a visibility consequence. The
        // refusal is here rather than on the executor because the whole batch can still be
        // rejected without effect at this point, which is what a 422 promises.
        VocabularyKind::Declared => Err(DecodeError(format!(
            "{body_name}: column '{}' carries value '{key}', which vocabulary '{vocabulary}' \
             does not list. Under `vocabulary = \"declared\"` there is no auto-mint: a category \
             carries properties and, through its postings, a visibility consequence, so a typo \
             must not create one (per-point-attributes §5)",
            declared.name
        ))),
        // **The key travels as a key.** This handler must not mint: two requests racing one novel
        // key would each draw, and that key would end up with two codes and its rows split
        // between them. The commit-window close resolves it — serially, against the live bindings
        // — and the row's scalar becomes the code there, before the WAL append.
        VocabularyKind::Discovered => Ok(WalScalar::Utf8(key.to_string())),
    }
}

/// A code at its column's declared width. `is_category_width` admits `u8`/`u16`/`u32` only, so the
/// fallthrough is `u32` — the widest, which cannot truncate a code the other two could hold.
pub(super) fn code_at(width: ScalarType, code: u32) -> WalScalar {
    match width {
        ScalarType::U8 => WalScalar::U8(code as u8),
        ScalarType::U16 => WalScalar::U16(code as u16),
        _ => WalScalar::U32(code),
    }
}

/// The body's record batches, whichever encoding carried it. A JSON body is coerced into one
/// record batch against the declared column types (`json::record_batch`) and then read by every
/// rule the Arrow batches are.
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

/// Whole-batch schema validation, before a single row is read: a batch whose columns do not match
/// the declarations has no effect at all, exactly as a duplicate 409 does. Returns the batch's
/// layer columns.
///
/// A name is the route's own, a declared scalar, a group-scoped family in `scoped`, or a
/// registered layer's, and is refused otherwise. A declared or scoped column present at the wrong
/// type is refused; one the batch omits is for the route to read.
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
            // **A group-scoped family, under its plain name** (`views.md` §5): the view is
            // known from the header, so the column is not qualified and the view decides
            // which of the family's columns the value lands in. `scoped` is empty for every
            // view outside a scope, so the refusal below is unchanged there — which is what
            // keeps a scoped column un-nameable on an entity-space batch.
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
    // **The scoped families' columns, checked on the declared ones' rule** (`views.md` §5): a
    // value decoded against the wrong declaration is a wrong value stored with no error anywhere.
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
/// value keys as `utf8`; every other declaration is read by the rule a build reads a points file
/// by.
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

/// One row's value of a column, read against `declared`: a scoped family's column is read against
/// [`scoped_as_declared`]'s declaration. `request_row` is the row's number in the request, for a
/// refusal to name.
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
                "{body_name}: row {request_row}, column '{}' carries {}, which is outside {}'s \
                 range {}..={}",
                declared.name,
                e.value,
                declared.arrow_type.arrow_type_name(),
                e.min,
                e.max
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

/// Contracts §1: a typed error, never a truncation -- see `EXTERNAL_ID_MAX_LEN`'s doc.
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

/// Parse `/control/ingest`'s body: one Arrow IPC stream, schema
/// `(external_id: binary, x: float32|float64, y: float32|float64, access: utf8, node_id: utf8?,
/// ...scalars)` (R5). The coordinate columns take either float width and the narrower is widened —
/// see [`coordinate_col`] for why the widening runs in that one direction. `node_id` is accepted
/// — so a well-formed client request is never rejected for including it — but not stored: `WalRow`
/// has no `node_id` field, because a buffered item has no row geometry until the next build and
/// `node_id` is a segment-column concept.
///
/// # The scalar tail is validated against `MANIFEST.declared_scalars`, and misalignment is a 422
///
/// A row's scalars are stored **positionally**, against the manifest's declared order — nothing
/// downstream carries a name. So a batch whose scalar columns are not within the declared set
/// cannot be read back correctly, and two defects are the same defect:
///
/// * a column the manifest does not declare;
/// * a declared column present at the wrong arrow type.
///
/// Each is refused with **422 naming the column** (contracts §3.1's "malformed request"), and the
/// scalar vector is built in **declared** order rather than schema order, which is what makes the
/// positional read safe. **A declared column the batch omits is absent in every row**
/// (`ingest.md` §7.1): the vector still takes the column's slot, holding its absence, so the
/// omission misaligns nothing, and a column declared at a running service is one an older
/// client's batches do not carry. Silently dropping a column would shorten the vector and shift
/// every later scalar by one: positional misalignment wearing a success's clothes, acknowledged
/// with a 200.
///
/// # A category arrives as its key, and the key is checked for membership
///
/// The expected type is [`DeclaredScalar::wire_type`], not the declared width: a category column
/// is `utf8` value keys on the wire, whatever width stores its codes. Codes are the server's to
/// assign (per-point-attributes §3.1, §5), so a caller supplying one would be the minting
/// authority, and the server could then guarantee neither the scatter nor never-reuse that §3.4
/// exists for.
///
/// **This is what makes a category's value checkable at all.** A code can only be range-checked —
/// a `u16` column accepted any `u16`, so an unassigned code, a `reserved` code or a typo was stored
/// with no error anywhere and the row carried a code no key explains. A key can be
/// membership-checked, and membership is the rule: an unknown key under `value_set = "closed"`
/// is a 422 naming the column and the key, whole batch without effect (declare-then-use, §5,
/// views §80).
///
/// It also makes a schema/client disagreement visible: a plain `u16` scalar and a `u16` category
/// are now different types on the wire, so a client that thinks a column is one when the bundle
/// says the other gets a 422 naming it rather than plausible integers stored as codes.
///
/// A **null** key is *absent* — [`ABSENT_CODE`], the reserved sentinel (§3.6). The **empty string**
/// is not: it is what an unset field and a client bug both produce, so it is refused rather than
/// folded into absence, which would accept the same defect silently.
///
/// # A column named for a declared layer is that point's artifacts
///
/// The acceptance rule is **reserved, or a declared attribute's name, or a declared layer's name**
/// (`artifacts-from-points.md` §6.2) — the third being what
/// [decision 0091](../../../docs/decisions/0091-build-is-ingest-into-an-empty-database.md) obliges:
/// a point may name its artifacts in a file, so it may name them on the wire. The cell is a key, or
/// a list of keys, on exactly the rules a build reads a member table by — `tessera_types::layer`
/// holds them, and holds them for both readers.
///
/// **The layer's own `name`, exactly as an attribute column is named for the attribute's `name`.**
/// `fields` on `[layer.members]` renames a *file's* columns and is build-only for the same reason
/// `source` is: it says where rows come from rather than what they mean.
///
/// Reserved names are matched first and declared scalars second, so a layer sharing a name with
/// either is read as the other — a layer called `x` cannot make the geometry column mean a cluster.
#[allow(clippy::too_many_arguments)]
pub(crate) fn parse_ingest_batch(
    encoding: BodyEncoding,
    body: &[u8],
    projection: Projection,
    declared: &[DeclaredScalar],
    // The **group-scoped** attribute families this view's batch may carry, under their plain
    // names — the families of the group that owns the view, in manifest order, and empty for
    // every view outside a scope (`views.md` §5). See the section on them in this function's doc.
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
        // Where this record batch's rows start in the request's own row numbering — what a
        // membership names, since the executor indexes one flat list of rows per batch id.
        let offset = items.len();

        let ext = optional_binary_col(body_name, &batch, "external_id")?;
        // **The spelling is checked before the columns are read**, so a batch that used the other
        // one meets a refusal naming what this view calls its axes rather than a bare "column 'x'
        // missing". Only fired where the right column is absent, so a `projection = "none"` view
        // whose declared scalars happen to include a `lon` is unaffected: this is the surface
        // every existing ingest uses.
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
        // **The transform runs here, at the boundary, before anything else looks at the numbers**
        // (`projections.md` §3) — the same place `tessera_build::input` runs it, which is what
        // makes a projected view ingestable rather than only buildable (decision 0091).
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
            // The artifacts this row names, read before its scalars so a malformed membership
            // column refuses the batch with nothing decoded into `items` — the whole-batch rule
            // every other refusal here is held to.
            for column in &memberships {
                tally.read(body_name, column, i, offset)?;
            }
            // Built in DECLARED order, not schema order — the vector is read back by position and
            // nothing downstream carries a name. **A declared column the batch omits is absent in
            // every row** (`ingest.md` §7.1): an omission misaligns nothing, and a column declared
            // at a running service is one an older client's batches do not carry.
            let mut scalars = Vec::with_capacity(declared.len());
            for (d, cells) in declared.iter().zip(&declared_cells) {
                let Some(cells) = cells else {
                    // The batch omits the column: this row's absence, on the family's own
                    // spelling (`scoped_absent`'s rule, for a declared scalar).
                    scalars.push(scoped_absent(&declared_as_scoped(d)));
                    continue;
                };
                scalars.push(cell(body_name, cells, i, offset + i, d, vocabularies)?);
            }
            // **The scoped tail, in the families' own order** — a second positional list rather
            // than more slots in the one above, because the two are indexed against different
            // declarations (`WalRow::scoped`). Absence takes each family's ordinary route: the
            // reserved code 0 for a category, `WalScalar::Null` for everything else, which is
            // decision 0064's presence bitmap.
            let mut scoped_values = Vec::with_capacity(scoped.len());
            let families = scoped.iter().zip(&scoped_declared).zip(&scoped_cells);
            for ((f, as_declared), cells) in families {
                let value = match cells {
                    // A family the batch does not mention: every row is absent in it, which is
                    // an ordinary state and not the omission a declared scalar's would be. A
                    // family has no bundle-wide column, so nothing downstream is misaligned by a
                    // batch that carries none of them.
                    None => scoped_absent(f),
                    Some(cells) => {
                        cell(body_name, cells, i, offset + i, as_declared, vocabularies)?
                    }
                };
                scoped_values.push(value);
            }
            // Contracts §3.4: `external_id` is optional. Neither a missing column nor a null
            // within the column is an error -- both simply mean this item has no caller-supplied
            // external id and is addressable only by its `tessera_id`.
            let external_id = match &ext {
                Some(arr) if !arr.is_null(i) => Some(arr.value(i).to_vec()),
                _ => None,
            };
            // Checked here, inside the whole-batch parse, so an over-length id anywhere in the
            // batch fails the parse before anything downstream (replay check, dedup, allocation,
            // WAL append) ever runs: the batch has no effect, exactly as a duplicate 409 must.
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

/// Decode one `POST /control/values` body (`ingest.md` §1.2, §1.4), in whichever encoding carried
/// it, into named columns and rows.
///
/// **The same rules as the ingest door, minus the ones about creating an entity.** A values row
/// carries no coordinates and no `access` list, and names its entity by exactly one of
/// `external_id` and `tessera_id`; a declared column present at the wrong type refuses the batch,
/// an undeclared name refuses it, and a layer column is a membership join.
///
/// **A group-scoped column is nameable only where `scoped` holds its family**, which the caller
/// resolves from the batch's own view header — so a batch that named no view meets the
/// undeclared-column refusal for one, and so does a batch whose view's key is in no scope
/// (`views.md` §5, decision 0116).
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
            // A values batch carries the subset of the schema the caller has, and a row of it may
            // leave a column out, which is that cell unfilled rather than a malformed row. The
            // ingest door's stricter reading is about a row that creates an entity, where a
            // half-carried column shifts the positional tail.
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

        // Which of the schema's names this batch's cells are, in one order for every row: the
        // declared columns, then the group-scoped families.
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
                    // **Required with a `tessera_id`, refused without it** — `/control/changes`'s
                    // rule, and it guards the same thing: an identifier's meaning depends on the
                    // set it was minted under, and a rotation would otherwise silently redirect
                    // the fill onto another entity.
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

/// The coordinate columns a batch for this view must *not* carry, each paired with what it should
/// have been called.
///
/// `x`/`y` and `lon`/`lat` are the only two spellings, so each view refuses exactly the other one
/// and the pair is total rather than a list that could be empty.
fn wrong_spellings(projection: Projection) -> [(&'static str, &'static str); 2] {
    match projection {
        Projection::None => [("lon", "x"), ("lat", "y")],
        _ => [("x", "lon"), ("y", "lat")],
    }
}

/// Project a batch's coordinate columns in place, returning how many rows the projection
/// **clipped** (`projections.md` §3, §7).
///
/// # Two things go wrong here and they are not the same thing
///
/// A coordinate outside WGS84's own range is **not a coordinate** and is refused, exactly as the
/// build refuses it (`projections.md` §2): the accepted input coordinate system is longitude
/// within ±180 and latitude within ±90, and a caller holding anything else converts before
/// arriving.
///
/// A latitude inside that range but outside the *projection's* domain — beyond ±85.0511287798066°
/// for `web_mercator` — is **clipped onto the frame's edge, counted, and never refused** (§7). The
/// same row builds, and a row a build accepts and an ingest rejects is a defect rather than a
/// policy. Clipping never earns a refusal at any proportion: a clipped point's position is the
/// projection's own domain boundary, which no choice of frame moves.
///
/// # Why the count is taken here and not downstream
///
/// The engine's out-of-frame check runs on what this function returns, and the frame's edge is
/// exactly where the quantisation rule says a point is *not* out of frame — so at the whole-world
/// frame that check structurally cannot see a single clipped row, however many there are. At a
/// sub-square frame the two do overlap, a clipped point landing on the *world's* edge and so
/// outside a frame that does not reach it; such a row is both clipped here and refused there,
/// which §7 states as correct rather than as an exception to carve out.
///
/// `Projection::None` returns without touching either column — the identity, bit for bit, which is
/// what keeps every existing ingest exactly as it was.
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
        clipped += u64::from(projection.is_clipped(*lat));
        let (px, py) = projection.forward(*lon, *lat);
        (*lon, *lat) = (px, py);
    }
    Ok(clipped)
}

/// A binary column that may be null-within (any row) or absent entirely (contracts §3.4:
/// `external_id` is optional). A present-but-wrong-typed column is still a typed error — only
/// "missing" and "null at this row" mean "no external id", never "this batch is malformed".
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

/// A coordinate column, as `f64` — **`float32` and `float64` are both accepted and the narrower is
/// widened**, which is the rule the build reads a points file's coordinate columns by
/// (`tessera_build::input`'s `read_f64_column`), stated here because ingest and build must not
/// disagree about which files can be loaded (decision 0091).
///
/// The widening direction is the only one: an `f64` column is never narrowed. A frame at zoom
/// offset *k* resolves `2^(8−k)` `f32` steps per cell, so past roughly offset 8 the narrowing would
/// decide the **cell** a point occupies (`projections.md` §6), and it would do so inside a request
/// the caller was acked for. A whole-world frame is served perfectly well by `float32`, which is
/// why the narrower width stays acceptable rather than being refused.
///
/// `offset` is where this record batch's rows start in the request, so a null names the request's
/// row.
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

/// The `access` column: one list of labels per row, `list<utf8>` or `large_list<utf8>`
/// (contracts §3.4, decision 0129).
///
/// **A list, because that is what the data is.** Each element is one label and is taken verbatim
/// — the plugin's [`tessera_plugin::Plugin::terms_of_labels`], the same call a build puts a
/// points file's term column through — so a label containing whatever separator a grammar might
/// have chosen is one term, as it is at the build. A scalar `utf8` column is refused at the
/// schema rather than read as a one-label row: it is the shape a separator grammar lived in, and
/// accepting it beside the list would leave two spellings for one column.
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

    /// Row `row`'s labels, verbatim and in order. **A null list and an empty list are one case,
    /// a row with no label** (decision 0133), which the view's declared default fills or, where
    /// none is declared, refuses with the count; the JSON door reads an absent or null `access`
    /// the same way, so the two doors agree. A whole column absent is still refused at the
    /// schema. A null element has no bytes to be a label and is refused naming the row.
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
        // One label per row, as a list (decision 0129).
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

    /// These fixtures declare no layer, so every column here is a scalar or a refusal —
    /// the membership column has its own cases in `tests/membership_column.rs`, over HTTP and
    /// against a registry that holds one.
    fn no_layers(_: &str) -> Option<tessera_types::layer::LayerDeclaration> {
        None
    }

    /// A known key becomes its **pinned** code at the column's declared width. The code is
    /// never re-derived from the data, so this is the whole of what the wire decides.
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

    /// **Declare-then-use** (§5, views §80): a category carries properties and a visibility
    /// consequence, so a typo must not create one. The refusal names both the column and the
    /// key, and the whole batch is without effect.
    #[test]
    fn an_unknown_key_is_refused_naming_the_column_and_the_key() {
        let err = parse(Arc::new(StringArray::from(vec!["k9-unit"])), false)
            .expect_err("an undeclared key is refused");
        let DecodeError(detail) = err;
        assert!(detail.contains("department"), "{detail}");
        assert!(detail.contains("k9-unit"), "{detail}");
    }

    /// **The hole this closes.** A code on the wire was accepted by range alone, so an
    /// unassigned code, a `reserved` code or a typo was stored with no error anywhere. The
    /// wire type is now `utf8`, so the same batch is a 422 naming the column.
    #[test]
    fn a_code_on_the_wire_is_refused_where_it_used_to_be_stored() {
        let err = parse(Arc::new(UInt8Array::from(vec![9u8])), false)
            .expect_err("a category is utf8 on the wire, whatever stores its codes");
        let DecodeError(detail) = err;
        assert!(detail.contains("department"), "{detail}");
        assert!(detail.contains("utf8"), "{detail}");
    }

    /// Null means *absent* — the reserved code 0, which is why a `u8` category holds 255
    /// values and not 256.
    #[test]
    fn a_null_key_is_absent() {
        let items = parse(
            Arc::new(StringArray::from(vec![None as Option<&str>])),
            true,
        )
        .expect("an item may carry no value for a column");
        assert_eq!(items[0].scalars[0], WalScalar::U16(ABSENT_CODE as u16));
    }

    /// The empty string is **not** absence. It is what an unset field and a client bug both
    /// produce, so folding it into code 0 would accept the same defect silently.
    #[test]
    fn the_empty_string_is_refused_rather_than_folded_into_absence() {
        let err = parse(Arc::new(StringArray::from(vec![""])), false)
            .expect_err("the empty string is not a value key");
        let DecodeError(detail) = err;
        assert!(detail.contains("department"), "{detail}");
    }

    /// **Under a discovered vocabulary a novel key travels as a key**, for the write executor
    /// to mint against the live bindings.
    ///
    /// The handler must not mint it here. Two requests racing one novel key would each draw,
    /// and that key would end up with two codes with its rows split between them — whichever
    /// binding survived would recolour the other's rows, silently. Windows close serially, so
    /// resolving there is what makes the two agree.
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

    /// A key the discovered vocabulary already binds resolves in the handler like any other —
    /// only the *novel* case needs the executor, so the common path costs no extra work.
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

    /// A plain scalar of the same width is unchanged and still arrives as an integer — so a
    /// client that thinks a column is a category when the bundle says otherwise gets a 422
    /// naming it, rather than plausible integers stored as codes.
    #[test]
    fn a_plain_scalar_is_unaffected_by_the_category_rule() {
        let items = parse(Arc::new(StringArray::from(vec!["ops"])), false).unwrap();
        assert_eq!(items[0].scalars[1], WalScalar::F32(1.0));
    }
}
