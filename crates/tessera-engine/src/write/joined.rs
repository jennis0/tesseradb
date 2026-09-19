//! The ingest join rule's readers of already-flushed values.

use tessera_types::{EntityId, TermId};

use crate::Generation;

/// One entity's record-blob row, decompressed **at most once** whatever how many blob-resident
/// columns ask for it (review finding F4).
///
/// `fields_of` decompresses the block the entity's row sits in, and the join rule's attribute arm
/// asks it once per blob-resident column: a schema with six such columns paid six decompressions of
/// one block per joining row. The row is the same for all of them, so it is read here and shared.
/// The outer `Option` is *not yet read*; the inner one is the blob's own answer, which is `None`
/// for an entity with no row and for a blob that could not be read alike — the same collapse
/// [`flushed_scalar_of`] documents, and for the same reason.
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
                // **The error's *kind*, never its `Display`** (**I10**). `RecordError::Malformed`
                // carries a detail string, and the blob's detail strings name the entity in six
                // spellings — its addressing checks are about *which* entity's row was found. The
                // byte-scanner sweeps logs as well as payloads, so this warning carries the
                // artefact and the class of defect, which is what an operator chasing a systematic
                // build or flush fault needs; the row that tripped it buys nothing an
                // entity-independent message does not. The drill-down propagates the same error as
                // a refusal, and that path may carry the detail: it reaches an operator's error
                // surface rather than the log the scanner reads.
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
/// it in — the join rule's attribute arm and its render backfill, once the entity's own row has
/// left the commit-window buffer (`views.md` §4).
///
/// **The label arm's shape, over three homes instead of one.** `entities/terms/` answers the label
/// question outright; an entity-scoped *value* has no single artefact, so this reads the home the
/// declaration puts it in and there are exactly three (records §3, decision 0068): the entity-space
/// value column where the column owes one (`index = true`, or a `derived` category's floor), the
/// record blob where it is blob-resident (text always; anything neither rendered nor
/// value-columned), and the hot column where `render = true` is the value's only store. The three
/// are exhaustive and, per column, the first that applies is the cheapest — only a render-only
/// column pays a row lookup.
///
/// The answer is returned as a [`tessera_lifecycle::WalScalar`] so the comparison at the call site
/// is the *same* equality the buffered arm makes against a buffered row's scalars: one comparison,
/// two sources, and the two arms cannot come to disagree about what "the same value" means.
///
/// `None` is *no value held*, and it is also *could not find out* — the two are one answer here
/// because the join it guards is inert in entity space either way (a joining row writes no
/// postings, no attribute column and no record field), so what an unreadable artefact costs is the
/// refusal, not the rule. Corruption is warned, never swallowed, and the warning names the artefact
/// rather than the entity (**I10**).
///
/// **A free function rather than a method, because the write executor needs this form**: it must
/// read the same generation its apply will clone from rather than re-loading the pointer under
/// itself. `blob` is the caller's per-row [`BlobRow`], so a schema's blob-resident columns share one
/// decompression.
///
/// **Server-side and control-plane.** Nothing derived from this reaches a client: the refusal names
/// the column, exactly as the buffered arm's does, and never the value on either side.
pub(crate) fn flushed_scalar_of(
    generation: &Generation,
    entity: EntityId,
    declared_index: usize,
    blob: &mut BlobRow,
) -> Option<tessera_lifecycle::WalScalar> {
    let manifest = &generation.bundle.manifest;
    let declared = manifest.declared_scalars.get(declared_index)?;
    let vocabularies = &manifest.vocabularies;
    // I9 caps entity ids at `u32::MAX`, and the same `expect` guards the drill-down's read. A
    // violated invariant is loud rather than a `None` the caller would read as "no value held"
    // and accept a mismatch under.
    let entity_raw = u32::try_from(entity.raw()).expect("entity ids are capped at u32::MAX by I9");

    let stored = if crate::filter::owes_value_column(declared, vocabularies) {
        generation
            .filter_columns
            .stored_value(&declared.name, entity_raw)
    } else if crate::filter::blob_resident(declared, vocabularies) {
        // The blob is keyed by entity and its rows are self-describing, so the field wanted is the
        // one tagged with this column's declared position (records §3). A malformed row refuses on
        // the drill-down path, which propagates it; here it is a lost report — warned once per
        // row by `BlobRow`, which is also what keeps this to one decompression however many
        // blob-resident columns the schema declares.
        blob.get(generation, entity_raw)
            .as_ref()?
            .iter()
            .find_map(|f| (f.tag as usize == declared_index).then(|| f.value.clone()))
    } else {
        crate::viewport::flushed_row_scalar(generation, entity, declared_index)
    }?;
    stored_as_wal(stored, declared)
}

/// [`Engine::flushed_terms`]'s body, over a generation the caller already holds.
///
/// **The write executor needs this form, and that is why it is not a method** — the same reason
/// [`flushed_scalar_of`] is not one. The join rule's label arm runs on the serial writer
/// (decision 0116), beside the generation its apply will clone from, and re-loading the pointer
/// under itself is exactly the race the relocation exists to close.
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

/// One already-flushed `(entity, attribute, key)` cell's value, at the shape a batch carries it in
/// — the scoped half of the join rule's attribute arm (`views.md` §5, decision 0116).
///
/// `owner_view` is the cell's address, `write::scoped_owner_view_of`'s answer, so a value written
/// through a sharing group's door and one written through the owner's are read back from the one
/// column. A family on no filter surface has no store to read and answers `None`, as does a `text`
/// family, whose extent is a dictionary and postings and holds no value per entity; both lose the
/// comparison rather than the rule, exactly as a blob-resident entity-scoped column does.
pub(crate) fn flushed_scoped_of(
    generation: &Generation,
    entity: EntityId,
    family: &tessera_store::manifest::ScopedScalar,
    owner_view: &str,
) -> Option<tessera_lifecycle::WalScalar> {
    if !crate::filter::scoped_is_filterable(family) {
        return None;
    }
    let entity = u32::try_from(entity.raw()).ok()?;
    let column = crate::filter::scoped_column_name(&family.name, owner_view);
    let stored = generation.filter_columns.stored_value(&column, entity)?;
    stored_as_wal(stored, &declared_of_scoped(family))
}

/// Does a **flushed** layer of this `(entity, attribute, key)` cell hold prose?
///
/// The `text` half of the scoped cell arm (`views.md` §5, decision 0116; review finding F1). A text
/// family has no per-entity value for [`flushed_scoped_of`] to answer with, so the cell arm asks
/// occupancy instead of equality and refuses a supplied string where the cell is occupied. `false`
/// for every other family, which has a value to compare, and for a family on no filter surface,
/// which has no column at all.
///
/// ⊘ **The build's base is not covered** — it writes no presence file
/// ([`crate::filter::FilterColumns::text_present`], issue #123) — so a cell whose only prose came
/// from the build reads as unoccupied. That is an under-refusal and it is stated rather than
/// hidden: the fix is the base's presence bitmap, not a change here.
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

/// A scoped family as the entity-scoped declaration the absence and comparison helpers take.
///
/// **The same transcription the ingest boundary makes** (`control.rs`'s `scoped_as_declared`): a
/// scoped column *is* an entity-scoped one — same types, same vocabulary, same absence rules — so
/// the two helpers that decide what "absent" and "the same value" mean take one shape and cannot
/// come to mean two things.
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

/// Is this value **no value at all** for `declared` — the join rule's "or be absent from the
/// batch" (`views.md` §4), asked of a supplied value and a stored one alike?
///
/// **Two spellings, because a category's absence is in band.** Every other family says absence
/// with [`tessera_lifecycle::WalScalar::Null`], having no bit pattern to spare; a vocabulary keeps
/// code 0 out of its value space precisely so a category can say it with a code
/// ([`tessera_store::vocabulary::ABSENT_CODE`], per-point-attributes §3.4), and the ingest parse
/// turns a null category cell into that code before either arm sees it. Reading only the `Null`
/// spelling made a joining batch that left a category null a `409` against an entity holding a
/// value, and a join carrying a value against an entity holding *none* a `409` as well, neither of
/// which the rule asks for.
///
/// One definition, because three callers ask it: the handler's comparison, on both sides, and the
/// executor's backfill.
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
        // window's close; a key is never absence — the empty string is refused upstream.
        _ => false,
    }
}

/// One stored value at the shape an ingest batch carries it in, so the join rule's attribute arm
/// compares like with like whichever home answered (`views.md` §4).
///
/// **Three normalisations, one per family whose storage type is not its wire type**, and each is a
/// storage fact rather than a presentation choice:
///
/// - **A `bool` stores as a byte** in a value column and as an Arrow boolean in the hot column, and
///   arrives as [`WalScalar::Bool`]; non-zero is `true`.
/// - **A `timestamp_us` stores as an `i64`** whose unit the declaration fixes, and arrives as
///   [`WalScalar::TimestampUs`]; the two are the same number.
/// - **A category stays a code**, and is deliberately *not* resolved to its key. The code is what
///   the batch carries by the time this comparison runs (`category_scalar` mints nothing and
///   resolves through the live bindings), the code is what the entity stores, and resolving both
///   ends through a vocabulary would make a rebinding — which never happens, codes being
///   never-reused — the only thing the extra work could ever detect. The declared width is the
///   one both sides are read at, so a `u8` column's code cannot compare unequal to itself because
///   one home widened it.
///
/// Every other family is its own storage type: a number byte-matches a number, and a keyword or a
/// text field matches on its **bytes** — decoded from this layer's sorted dictionary where the
/// value column holds an ordinal (**I10**: an ordinal is an index internal and never the unit of
/// comparison across a flush boundary, because two flushes number the same key differently).
///
/// `None` where the stored value cannot be read at the declared type at all, which is a malformed
/// bundle rather than a caller's error and so loses the report rather than refusing the batch.
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
        // ⊘ Lists land with epic 3's multi surface; no writer produces one, and a reader that met
        // one would be looking at a future format — no answer, not a guess.
        (_, RV::List(_)) => return None,
    })
}
