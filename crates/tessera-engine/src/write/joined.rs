//! The ingest join rule's readers of already-flushed values.

use tessera_types::{EntityId, TermId};

use crate::Generation;

/// One entity's record-blob row, decompressed at most once however many blob-resident columns
/// ask for it in one join. The outer `Option` is *not yet read*; the inner one is the blob's own
/// answer, `None` for a missing row or an unreadable blob alike.
#[derive(Default)]
pub(crate) struct BlobRow(Option<Option<Vec<tessera_filter::RecordField>>>);

impl BlobRow {
    fn get(
        &mut self,
        generation: &Generation,
        entity: u32,
    ) -> &Option<Vec<tessera_filter::RecordField>> {
        self.0.get_or_insert_with(|| {
            match generation.filter_columns.records().fields_of(entity) {
                Ok(fields) => fields,
                // The error's kind, never its `Display`, which would name the entity: this
                // warning carries the artefact and the defect class for an operator chasing a
                // build or flush fault. The drill-down propagates the full error as a refusal.
                Err(e) => {
                    let kind = match &e {
                        tessera_filter::RecordError::Io(io) => io.kind().to_string(),
                        tessera_filter::RecordError::Malformed(_) => "malformed".to_string(),
                    };
                    tracing::warn!(
                        artefact = "attrs/record",
                        kind = %kind,
                        "the record blob could not answer, so the join rule's attribute arm has \
                         nothing to compare a blob-resident column against and this batch's joins \
                         are accepted unchecked (views §4). The artefact is a build or flush \
                         defect; a fold rewrites it."
                    );
                    None
                }
            }
        })
    }
}

/// An already-flushed entity's stored value for one declared column, at the shape a batch carries
/// it in — the join rule's attribute arm and its render backfill. A column's value lives in one
/// of three homes: the entity-space value column where the column owes one, the record blob
/// where it is blob-resident, or the hot column where `render = true` is its only store. `None`
/// is both *no value held* and *could not find out*, and corruption is warned, naming the
/// artefact, never the entity.
///
/// A free function, not a method: the write executor must read the same generation its apply
/// will clone from rather than re-loading the pointer.
pub(crate) fn flushed_scalar_of(
    generation: &Generation,
    entity: EntityId,
    declared_index: usize,
    blob: &mut BlobRow,
) -> Option<tessera_lifecycle::WalScalar> {
    let manifest = &generation.bundle.manifest;
    let declared = manifest.declared_scalars.get(declared_index)?;
    let vocabularies = &manifest.vocabularies;
    // Entity ids are capped at `u32::MAX`; a violated invariant is loud rather than a `None`
    // read as "no value held".
    let entity_raw = u32::try_from(entity.raw()).expect("entity ids are capped at u32::MAX by I9");

    let stored = if crate::filter::owes_value_column(declared, vocabularies) {
        generation
            .filter_columns
            .stored_value(&declared.name, entity_raw)
    } else if crate::filter::blob_resident(declared, vocabularies) {
        // The blob's rows are self-describing; a malformed row is a lost report here, warned
        // once per row by `BlobRow`.
        blob.get(generation, entity_raw)
            .as_ref()?
            .iter()
            .find_map(|f| (f.tag as usize == declared_index).then(|| f.value.clone()))
    } else {
        crate::viewport::flushed_row_scalar(generation, entity, declared_index)
    }?;
    stored_as_wal(stored, declared)
}

/// [`Engine::flushed_terms`]'s body. Not a method: the label arm runs on the serial writer,
/// beside the generation its apply will clone from, and re-loading the pointer under itself is
/// the race this closes.
pub(crate) fn flushed_terms_of(generation: &Generation, entity: EntityId) -> Option<Vec<TermId>> {
    let entity = u32::try_from(entity.raw()).ok()?;
    let terms = match generation.filter_columns.entity_terms().terms_of(entity) {
        Ok(terms) => terms?,
        Err(e) => {
            tracing::warn!(
                error = %e,
                "the entity->term transpose could not answer, so the join rule's label arm \
                 has nothing to compare against and this batch's joins are accepted \
                 unchecked (views §4). The artefact is a build or flush defect and the error \
                 names the file; a fold rewrites it."
            );
            return None;
        }
    };
    Some(terms.into_iter().map(TermId::new).collect())
}

/// One already-flushed `(entity, attribute, key)` cell's value, at the shape a batch carries it
/// in. `owner_view` is the cell's address, so a value written through a sharing group's door and
/// one through the owner's are read from the one column. A family with no value column, or a
/// `text` family (no per-entity value), answers `None`.
///
/// The condition is the **value column**, not the filter licence: a family declaring neither
/// `index` nor `render` still has one, so its cell holds a value to compare against.
pub(crate) fn flushed_scoped_of(
    generation: &Generation,
    entity: EntityId,
    family: &tessera_store::manifest::ScopedScalar,
    owner_view: &str,
) -> Option<tessera_lifecycle::WalScalar> {
    if !crate::filter::scoped_has_value_column(family) {
        return None;
    }
    let entity = u32::try_from(entity.raw()).ok()?;
    let column = crate::filter::scoped_column_name(&family.name, owner_view);
    let stored = generation.filter_columns.stored_value(&column, entity)?;
    stored_as_wal(stored, &declared_of_scoped(family))
}

/// Does a flushed layer of this `(entity, attribute, key)` cell hold prose? A text family has no
/// per-entity value for [`flushed_scoped_of`], so this asks occupancy instead of equality.
/// `false` for every other family and for one with no filter surface.
///
/// Not built yet: the build's base writes no presence file, so a cell whose only prose came from
/// the build reads as unoccupied.
pub(crate) fn flushed_scoped_text_present(
    generation: &Generation,
    entity: EntityId,
    family: &tessera_store::manifest::ScopedScalar,
    owner_view: &str,
) -> bool {
    if !crate::filter::scoped_is_filterable(family) {
        return false;
    }
    let Ok(entity) = u32::try_from(entity.raw()) else {
        return false;
    };
    let column = crate::filter::scoped_column_name(&family.name, owner_view);
    generation.filter_columns.text_present(&column, entity)
}

/// A scoped family as the entity-scoped declaration the absence and comparison helpers take: same
/// types, same vocabulary, same absence rules, so the two helpers cannot come to mean two things.
pub(crate) fn declared_of_scoped(
    family: &tessera_store::manifest::ScopedScalar,
) -> tessera_store::manifest::DeclaredScalar {
    tessera_store::manifest::DeclaredScalar {
        name: family.name.clone(),
        arrow_type: family.arrow_type,
        vocabulary: family.vocabulary.clone(),
        analyser: family.analyser.clone(),
        index: family.index,
        render: family.render,
    }
}

/// Is this value no value at all for `declared`? A category's absence is in band: a vocabulary
/// keeps code 0 out of its value space so a category can say absence with a code
/// ([`tessera_store::vocabulary::ABSENT_CODE`]), and ingest turns a null category cell into that
/// code before either arm sees it. Every other family says absence with
/// [`tessera_lifecycle::WalScalar::Null`].
pub(crate) fn scalar_is_absent(
    value: &tessera_lifecycle::WalScalar,
    declared: &tessera_store::manifest::DeclaredScalar,
) -> bool {
    use tessera_lifecycle::WalScalar as WS;
    if matches!(value, WS::Null) {
        return true;
    }
    if declared.vocabulary.is_none() {
        return false;
    }
    let absent = tessera_store::vocabulary::ABSENT_CODE;
    match value {
        WS::U8(c) => u32::from(*c) == absent,
        WS::U16(c) => u32::from(*c) == absent,
        WS::U32(c) => *c == absent,
        // A novel key on a `discovered` vocabulary travels as its key and is minted at the commit
        // window's close; a key is never absence, since the empty string is refused upstream.
        _ => false,
    }
}

/// One stored value at the shape an ingest batch carries it in, so the join rule's attribute arm
/// compares like with like whichever home answered. Three families store a different type than
/// they carry on the wire: a `bool` as a byte or an Arrow boolean; a `timestamp_us` as an `i64`,
/// same unit; a category as its code, never resolved to its key, since codes are never reused.
/// Every other family matches its own storage type, and a keyword or text field matches on its
/// bytes — an ordinal never compares across a flush boundary, since two flushes number the same
/// key differently.
///
/// `None` where the stored value cannot be read at the declared type, a malformed bundle rather
/// than a caller's error.
fn stored_as_wal(
    value: tessera_filter::RecordValue,
    declared: &tessera_store::manifest::DeclaredScalar,
) -> Option<tessera_lifecycle::WalScalar> {
    use tessera_filter::RecordValue as RV;
    use tessera_lifecycle::WalScalar as WS;
    use tessera_spatial::tiler::ScalarType;

    if declared.vocabulary.is_some() {
        let code = match value {
            RV::U8(c) => u32::from(c),
            RV::U16(c) => u32::from(c),
            RV::U32(c) => c,
            _ => return None,
        };
        return Some(match declared.arrow_type {
            ScalarType::U8 => WS::U8(code as u8),
            ScalarType::U16 => WS::U16(code as u16),
            _ => WS::U32(code),
        });
    }
    Some(match (declared.arrow_type, value) {
        (ScalarType::Bool, RV::U8(x)) => WS::Bool(x != 0),
        (ScalarType::Bool, RV::Bool(b)) => WS::Bool(b),
        (ScalarType::TimestampUs, RV::I64(x)) | (ScalarType::TimestampUs, RV::TimestampUs(x)) => {
            WS::TimestampUs(x)
        }
        (_, RV::U8(x)) => WS::U8(x),
        (_, RV::U16(x)) => WS::U16(x),
        (_, RV::U32(x)) => WS::U32(x),
        (_, RV::U64(x)) => WS::U64(x),
        (_, RV::I8(x)) => WS::I8(x),
        (_, RV::I16(x)) => WS::I16(x),
        (_, RV::I32(x)) => WS::I32(x),
        (_, RV::I64(x)) => WS::I64(x),
        (_, RV::F32(x)) => WS::F32(x),
        (_, RV::F64(x)) => WS::F64(x),
        (_, RV::Bool(b)) => WS::Bool(b),
        (_, RV::TimestampUs(x)) => WS::TimestampUs(x),
        (_, RV::Utf8(s)) => WS::Utf8(s),
        // Lists land with a later format; no writer produces one, and a reader that met one would
        // be looking at a future format.
        (_, RV::List(_)) => return None,
    })
}
