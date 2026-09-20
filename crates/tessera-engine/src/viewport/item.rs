//! The item drill-down: one visible item's record, the views it sits in and its scoped values.

use super::*;
use super::out::flat_families;

/// `POST /v1/items/{handle}`'s payload: a visible item's full record — every declared field that
/// carries a value, by declared name — plus its caller-supplied external id, if it has one. Names,
/// not tags: a blob field's tag is a declaration position and an index internal, resolved to the
/// declared name engine-side. No tag, no entity id and no blob detail crosses the trust boundary.
/// A category field carries its vocabulary key, never its code.
#[derive(Debug, Clone, PartialEq)]
pub struct ItemOut {
    /// Present fields only, in declaration order — an absent field is absent, not null.
    pub fields: Vec<ItemField>,
    pub external_id: Option<Vec<u8>>,
    /// The satisfied terms only: the intersection of this item's own term set with the asking
    /// session's satisfied set, presented through the plugin, sorted by the presented string.
    /// Never the item's full label set. Taken against [`Session::satisfied_descriptors`], which
    /// holds only the descriptors the credential presented, so a term outside the grant has no
    /// name to be served under.
    pub labels: Vec<String>,
    /// The views this item holds a row in that this session may reach, sorted by id, each with
    /// the position that view places it at. A view the gate refuses is absent, exactly as a view
    /// nobody declared is: the array is built from [`crate::Session::visible_views`], so it can
    /// never become the one place a gate-failed view is named — an item held only in unreachable
    /// views and an item in no view at all both serve empty.
    pub views: Vec<ItemView>,
    /// The group-scoped attribute values this principal may see, one entry per family, sorted by
    /// family name. Keyed by the group's key, and gate-filtered per key on the same set as
    /// `views`: a key whose views this principal cannot reach is absent, and a family with no
    /// reachable key is absent whole. A `text` family is the one absent kind outright, having no
    /// per-entity value slot to read.
    pub scoped: Vec<ItemScoped>,
}

/// One view a drill-down's item holds a row in, and where that view puts it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ItemView {
    /// The view's id — a plain view's name or `<group>:<key>`, the same address `/v1/meta` and
    /// every viewer verb use.
    pub id: String,
    /// The horizontal axis in this view's own grid units, 32-bit fixed point against the frame
    /// `/v1/meta` publishes for this view. Deinterleaved server-side because a JSON number cannot
    /// hold a 64-bit code exactly.
    pub x: u32,
    /// The vertical axis, on [`Self::x`]'s terms.
    pub y: u32,
}

/// One group-scoped attribute family's values for a drill-down's item, keyed by the group's key.
/// No group name here: `/v1/meta`'s `scoped_scalars` already says which group it is scoped to.
#[derive(Debug, Clone, PartialEq)]
pub struct ItemScoped {
    /// The family's name, as a filter leaf spells it before any pin.
    pub name: String,
    /// The values this item carries, by key, sorted by key. A key with no value is absent rather
    /// than null; a key this principal cannot reach is absent for a different reason, and the two
    /// are one shape.
    pub values: Vec<(String, ScalarOut)>,
}

/// One declared field of a drill-down record: the column's declared name and its value.
#[derive(Debug, Clone, PartialEq)]
pub struct ItemField {
    pub name: String,
    pub value: ScalarOut,
}

impl Engine {
    /// Is `entity` visible to `session` under `generation` — the one bit `/v1/items` needs. An
    /// entity-space question: three constant-time probes, no `RowProjection` constructed or
    /// consulted, so this costs the same whether `entity` exists and is visible, exists and is
    /// not, or does not exist at all. Takes the fragment as an argument: a session's own fragment
    /// goes stale at every flush, and answering from a stale one would report a flushed item as
    /// invisible while the viewport beside it drew the mark.
    pub fn visible_to(
        &self,
        fragment: &FrozenFragment,
        session: &Session,
        generation: &Generation,
        entity: EntityId,
    ) -> bool {
        visible_to(
            fragment,
            session.satisfied(),
            &generation.overlay,
            &generation.buffer,
            entity,
        )
    }

    /// The drill-down's `labels` array: this entity's own terms, intersected with the session's
    /// satisfied set, presented through the plugin. Satisfied-only, twice over: the intersection
    /// reads [`Session::satisfied_descriptors`], which holds exactly the descriptors the
    /// credential presented, plus `public`, so there is no descriptor in scope for a term outside
    /// the grant even if the intersection were written wrongly. Nothing here reads the bundle
    /// dictionary, so there is no route from an ordinal to a descriptor that bypasses the session.
    /// Reached only after the visibility verdict, like every other read in [`Engine::item`]: the
    /// transpose is never probed for an entity the principal cannot see. An entity the transpose
    /// does not hold answers `[]` rather than refusing: it hides a label rather than inventing
    /// one, reachable only while a prefix predates the transpose. Not built: plugin routing
    /// beyond the built-in one. `present_terms` is answered by
    /// `builtin:passthrough`, whose descriptors are the caller's own label strings.
    fn labels_for(
        &self,
        generation: &Generation,
        session: &Session,
        entity: u32,
    ) -> Result<Vec<String>> {
        let Some(terms) = generation
            .filter_columns
            .entity_terms()
            .terms_of(entity)
            .map_err(EngineError::Store)?
        else {
            return Ok(Vec::new());
        };
        let descriptors: Vec<Vec<u8>> = terms
            .into_iter()
            .filter_map(|term| {
                session
                    .satisfied_descriptors()
                    .get(&TermId::new(term))
                    .cloned()
            })
            .collect();
        if descriptors.is_empty() {
            return Ok(Vec::new());
        }
        let mut labels = self
            .plugin
            .present_terms(&descriptors)
            .map_err(EngineError::Plugin)?;
        // One string per descriptor: more strings would put on the wire a label answering to no
        // term this session satisfies. Fewer is refused too: positional is the contract, so a
        // short list means the caller cannot say which label it failed to present.
        if labels.len() != descriptors.len() {
            return Err(EngineError::Plugin(tessera_plugin::PluginError::Malformed(
                format!(
                    "present_terms returned {} strings for {} descriptors; the mapping is \
                     positional, and a longer list would serve a label answering to no term this \
                     session satisfies",
                    labels.len(),
                    descriptors.len()
                ),
            )));
        }
        labels.sort_unstable();
        labels.dedup();
        Ok(labels)
    }

    /// `POST /v1/items/{handle}`: validate `idset` if the caller sent one, invert `id` to its
    /// entity, test visibility in entity space, and only then locate a row and read its
    /// scalars/external id.
    /// `idset` is checked against the same generation this call loads for the lookup below, never
    /// a separate `Engine::meta()` call: a generation swap between two separate calls could
    /// validate the idset against one generation and serve the lookup from another. Returns
    /// `Ok(None)` both when `id` names nothing in this bundle and when it names an item the
    /// principal may not see: one outcome from one code path.
    /// The timing channel is closed, not narrowed. The idset check is entity-independent and does
    /// not read `id`. Inversion is a pure function. The visibility test that follows is an
    /// entity-space question — three constant-time probes — and is the same three probes for an
    /// identifier that names nothing and one that names an invisible item: no `RowProjection` is
    /// constructed or read, so there is no per-ID cost to correlate against. A row is located only
    /// after the answer is already visible, and the sidecar is read only after that. Returns `Err`
    /// rather than a fail-open `None`: a digest mismatch, an out-of-order extent or a short
    /// locator is a `500`, never an item served with `external_id: null`. This does not reopen the
    /// timing channel, since the sidecar is touched only for an item already established visible.
    pub fn item(
        &self,
        session: &Session,
        id: TesseraId,
        idset: Option<u32>,
    ) -> Result<Option<ItemOut>> {
        let generation = self.generation.load_full();

        // Checked against the generation this call already loaded above; see this method's doc.
        if let Some(e) = idset {
            if e != generation.bundle.manifest.identity.idset {
                return Err(EngineError::StaleIdSet);
            }
        }

        let (shard, entity) = self.identity_key.invert(id);
        if shard != generation.bundle.manifest.identity.shard_id {
            return Ok(None);
        }

        // Brought forward before the visibility test: a fragment resolved per-entity would make
        // the cost depend on which entity was asked for. The served fragment, not the live one:
        // rebuilding at the live watermark measures ~200 ms per credential per publication, and
        // would answer from a different watermark than the viewport beside it serves from. Falls
        // back to a build only when this session has no resident entry, scoped to this
        // generation's prefix so a fold's flip cannot answer from a replaced term index.
        let fragment = match self
            .row_projection_cache
            .freshest_fragment(session.token_id(), &generation.prefix)
        {
            Some(fragment) => fragment,
            None => self.fragment_for(session, &generation)?,
        };

        // ONE BIT, in entity space, O(1), before anything is looked up in row space.
        if !self.visible_to(&fragment, session, &generation, entity) {
            return Ok(None);
        }

        // Visible. Now, and only now, find the row: every read below — the row lookup, the
        // entity-space value reads, the blob block read and the sidecar read — is reachable only
        // for an item already established visible, so none of that cost is probeable by an
        // attacker. The allocator caps entity ids at `u32::MAX`, and inversion produced this one
        // from a 32-bit half; checked rather than cast so a violated invariant fails loudly.
        let entity_raw =
            u32::try_from(entity.raw()).expect("entity ids are capped at u32::MAX by I9");
        let rows = rows_of(&generation, entity)?;
        let views = item_views(&rows, session.visible_views());
        let Some(&(_view, segment, local)) = row_to_read(&rows, session.visible_views()) else {
            // Visible in entity space but with no row anywhere: a buffered item awaiting flush.
            // Same `Ok(None)`, same 404 — it has no geometry to return.
            return Ok(None);
        };

        // The scoped values, gate-filtered by the same set and keyed by the group's key. See
        // [`scoped_values_of`].
        let scoped = scoped_values_of(&generation, session.visible_views(), entity_raw);
        let fields = record_fields(&generation, segment, local, entity_raw)?;
        Ok(Some(ItemOut {
            fields,
            labels: self.labels_for(&generation, session, entity_raw)?,
            views,
            scoped,
            // Propagate, never swallow: the same `EngineError::Store` wrapping every other
            // store-backed call in this crate uses. Against the generation this request loaded,
            // never a second `load()`.
            external_id: self
                .external_id_of_in(&generation, entity)
                .map_err(EngineError::Store)?,
        }))
    }
}

/// Every view this item holds a row in, resolved in one pass, sorted by view id. The `views`
/// array is the gate-filtered part of this list, so a second walk would be a second chance to
/// disagree about which rows exist. An O(1) bounds-checked slot read per view rather than a scan,
/// so the whole array costs O(views). Sorted, because the maps walked here are hash maps, and
/// each view id appears once: an entity lives in exactly one partition.
fn rows_of(
    generation: &Generation,
    entity: EntityId,
) -> Result<Vec<(&str, &SegmentData, usize)>> {
    let mut rows: Vec<(&str, &SegmentData, usize)> = Vec::new();
    for partition in generation.bundle.partitions.values() {
        for (view, view_data) in &partition.views {
            let Some((segment, local)) = segment_row_of(view, view_data, entity)? else {
                continue;
            };
            rows.push((view.as_str(), segment, local));
        }
    }
    rows.sort_unstable_by(|a, b| a.0.cmp(b.0));
    Ok(rows)
}

/// The positions, gate-filtered: one entry per view of this item's that the session may reach,
/// nothing for the views it may not. A view failing the gate is absent exactly as a view nobody
/// declared is, so the array never becomes the one place a gate-failed view is named. The
/// position decodes against the frame `/v1/meta` publishes for that view and no other.
fn item_views(
    rows: &[(&str, &SegmentData, usize)],
    visible: &crate::gate::VisibleViews,
) -> Vec<ItemView> {
    rows.iter()
        .filter(|(view, _, _)| visible.contains_view(view))
        .map(|&(view, segment, local)| {
            let (x, y) = tessera_spatial::unsplit32(
                tessera_types::MortonCode::new(segment.morton.u32()[local]),
                segment.columns.residual()[local],
            );
            ItemView {
                id: view.to_string(),
                x,
                y,
            }
        })
        .collect()
}

/// The row the record's homes read: one of a view this principal may reach, where one exists.
/// Rows are ordered by view id, so without this the field values would come from whichever view
/// sorts first, a gate-failed one included. Nothing is disclosed either way, since home 1 reads
/// the declared render scalars, which hold the same value in every view. A point held only in
/// views this principal cannot reach is still served, and its record is what it always was.
fn row_to_read<'a>(
    rows: &'a [(&str, &SegmentData, usize)],
    visible: &crate::gate::VisibleViews,
) -> Option<&'a (&'a str, &'a SegmentData, usize)> {
    rows.iter()
        .find(|(view, _, _)| visible.contains_view(view))
        .or_else(|| rows.first())
}

/// The record, assembled from its three homes: render fields from the row's scalar tail, indexed
/// and category fields from their entity-space structures, and everything else from one record
/// blob read. No tag, no entity id and no blob internal reaches the wire. Reached only after
/// [`Engine::item`]'s visibility verdict, like every other read there.
fn record_fields(
    generation: &Generation,
    segment: &SegmentData,
    local: usize,
    entity: u32,
) -> Result<Vec<ItemField>> {
    let manifest = &generation.bundle.manifest;
    // Render columns only in the row read: homes 2 and 3 read entity-space and blob columns.
    let render_scalars: Vec<_> = manifest.render_scalars().cloned().collect();

    // One value slot per declared column; a column no home holds a value in stays `None`.
    let mut values: Vec<Option<ScalarOut>> = vec![None; manifest.declared_scalars.len()];

    // Home 1: the row, via the same `resolve_scalars` the viewport gather uses.
    let resolved = resolve_scalars(segment, &render_scalars);
    for (slot, declared_index) in manifest.render_indices().enumerate() {
        let Some(view) = &resolved[slot] else {
            continue;
        };
        let d = &manifest.declared_scalars[declared_index];
        // Absence is the presence bitmap beside the column, never a zero in it. A category
        // needs no bitmap: its absence is the reserved code.
        if d.vocabulary.is_none()
            && !segment
                .columns
                .presence(&d.name)
                .contains(u32::try_from(local).expect("a segment holds fewer than 2^32 rows"))
        {
            continue;
        }
        values[declared_index] = row_field_out(view, local, d, &generation.vocabularies);
    }

    // Home 2: entity space, every non-rendered column with a value column.
    for (declared_index, d) in manifest.declared_scalars.iter().enumerate() {
        if d.render || values[declared_index].is_some() {
            continue;
        }
        if let Some(stored) = generation.filter_columns.stored_value(&d.name, entity) {
            values[declared_index] = stored_field_out(
                stored,
                d.arrow_type,
                d.vocabulary.as_deref(),
                &generation.vocabularies,
            );
        }
    }

    // Home 3: the record blob, one block read. Fail-closed: a malformed row, a bad tag or an
    // addressing defect refuses the request rather than serving a neighbour's field.
    if let Some(blob_fields) = generation
        .filter_columns
        .records()
        .fields_of(entity)
        .map_err(|e| EngineError::Malformed(e.to_string()))?
    {
        for field in blob_fields {
            let declared_index = field.tag as usize;
            let Some(d) = manifest.declared_scalars.get(declared_index) else {
                return Err(EngineError::Malformed(format!(
                    "a record-blob row carries field tag {} where the schema \
                     declares {} columns; the blob and the manifest disagree",
                    field.tag,
                    manifest.declared_scalars.len()
                )));
            };
            if values[declared_index].is_none() {
                values[declared_index] = stored_field_out(
                    field.value,
                    d.arrow_type,
                    d.vocabulary.as_deref(),
                    &generation.vocabularies,
                );
            }
        }
    }

    Ok(manifest
        .declared_scalars
        .iter()
        .zip(values)
        .filter_map(|(d, value)| {
            value.map(|value| ItemField {
                name: d.name.clone(),
                value,
            })
        })
        .collect())
}

/// One render column's drill-down value, read from the row: a category's code resolved to its
/// key — code 0, the absent sentinel — and every other family as stored. A rendered number's
/// absence is still stored as the type's zero, so a numeric zero here may be real or absent; this
/// reports the stored value rather than inventing a rule.
fn row_field_out(
    view: &ScalarSlice<'_>,
    idx: usize,
    d: &DeclaredScalar,
    vocabularies: &Vocabularies,
) -> Option<ScalarOut> {
    if d.vocabulary.is_some() {
        let code = match view {
            ScalarSlice::U8(s) => s[idx] as u32,
            ScalarSlice::U16(s) => s[idx] as u32,
            ScalarSlice::U32(s) => s[idx],
            // A category is one of the three widths; anything else is a malformed tail.
            _ => return None,
        };
        return category_key_out(code, d.vocabulary.as_deref(), vocabularies);
    }
    // Generated for the flat members; `Bool` and `Utf8` are hand-read, not flat slices.
    macro_rules! out {
        ($(($v:ident, $t:ty)),* $(,)?) => {
            match view {
                $(ScalarSlice::$v(s) => ScalarOut::$v(s[idx]),)*
                ScalarSlice::Bool(a) => ScalarOut::Bool(a.value(idx)),
                ScalarSlice::Utf8(a) => ScalarOut::Utf8(a.value(idx).to_string()),
            }
        };
    }
    Some(flat_families!(out))
}

/// The hot-column value one already-flushed entity carries for `declared_index`, or `None` where
/// no view holds a row for it, the column is not in the render tail, or the row's presence bitmap
/// says the slot is empty. This home exists because `render = true, index = false` over a
/// non-`derived` vocabulary owes no value column and is not blob-resident, so the hot column is
/// the value's only store. Every view is scanned, and one holding no value is skipped rather than
/// answering, because views can hold different tails lawfully and absence must never pre-empt a
/// view that holds something. `None` means no view holds a present value. A malformed segment set
/// is skipped too; where no view answers, the join is accepted unchecked. The warning names the
/// view and never the entity.
pub(crate) fn flushed_row_scalar(
    generation: &Generation,
    entity: EntityId,
    declared_index: usize,
) -> Option<tessera_filter::RecordValue> {
    use tessera_filter::RecordValue as RV;

    let manifest = &generation.bundle.manifest;
    // The slot in the render tail; a column that is not rendered has no hot-column home.
    let slot = manifest
        .render_indices()
        .position(|i| i == declared_index)?;
    let d = manifest.declared_scalars.get(declared_index)?;
    let render_scalars: Vec<_> = manifest.render_scalars().cloned().collect();

    for partition in generation.bundle.partitions.values() {
        for (view, view_data) in &partition.views {
            let resolved_row = match segment_row_of(view, view_data, entity) {
                Ok(resolved) => resolved,
                Err(e) => {
                    tracing::warn!(
                        error = %e,
                        view = %view,
                        "a view's segment set could not be resolved, so the join rule's attribute \
                         arm cannot read a render-only column from this view (views §4). The scan \
                         continues; if no view answers, the batch's joins are accepted unchecked. \
                         The artefact is a build or flush defect; a fold rewrites it."
                    );
                    continue;
                }
            };
            let Some((segment, local)) = resolved_row else {
                continue;
            };
            // Absence is the presence bitmap, never a zero in it; a category has none.
            let Ok(local_row) = u32::try_from(local) else {
                continue;
            };
            if d.vocabulary.is_none() && !segment.columns.presence(&d.name).contains(local_row) {
                continue;
            }
            let resolved = resolve_scalars(segment, &render_scalars);
            let Some(Some(view_slice)) = resolved.get(slot) else {
                continue;
            };
            let read = match view_slice {
                ScalarSlice::Bool(a) if local < arrow::array::Array::len(*a) => {
                    Some(RV::Bool(a.value(local)))
                }
                ScalarSlice::Utf8(a) if local < arrow::array::Array::len(*a) => {
                    Some(RV::Utf8(a.value(local).to_string()))
                }
                ScalarSlice::Bool(_) | ScalarSlice::Utf8(_) => None,
                ScalarSlice::U8(s) => s.get(local).copied().map(RV::U8),
                ScalarSlice::U16(s) => s.get(local).copied().map(RV::U16),
                ScalarSlice::U32(s) => s.get(local).copied().map(RV::U32),
                ScalarSlice::U64(s) => s.get(local).copied().map(RV::U64),
                ScalarSlice::I8(s) => s.get(local).copied().map(RV::I8),
                ScalarSlice::I16(s) => s.get(local).copied().map(RV::I16),
                ScalarSlice::I32(s) => s.get(local).copied().map(RV::I32),
                ScalarSlice::I64(s) => s.get(local).copied().map(RV::I64),
                ScalarSlice::F32(s) => s.get(local).copied().map(RV::F32),
                ScalarSlice::F64(s) => s.get(local).copied().map(RV::F64),
                ScalarSlice::TimestampUs(s) => s.get(local).copied().map(RV::TimestampUs),
            };
            if read.is_some() {
                return read;
            }
        }
    }
    None
}

/// One stored value's drill-down form, for the entity-space and blob homes: the storage-typed
/// [`tessera_filter::RecordValue`] adapted through the declaration — a category code to its key,
/// a `bool`'s `u8` storage back to `bool`, a `timestamp_us`'s `i64` back to its unit. Over the two
/// facts rather than the declaration, since a group-scoped family has no [`DeclaredScalar`].
fn stored_field_out(
    value: tessera_filter::RecordValue,
    arrow_type: ScalarType,
    vocabulary: Option<&str>,
    vocabularies: &Vocabularies,
) -> Option<ScalarOut> {
    use tessera_filter::RecordValue as RV;
    if vocabulary.is_some() {
        let code = match value {
            RV::U8(c) => c as u32,
            RV::U16(c) => c as u32,
            RV::U32(c) => c,
            _ => return None,
        };
        return category_key_out(code, vocabulary, vocabularies);
    }
    Some(match (arrow_type, value) {
        (ScalarType::Bool, RV::U8(x)) => ScalarOut::Bool(x != 0),
        (ScalarType::Bool, RV::Bool(b)) => ScalarOut::Bool(b),
        (ScalarType::TimestampUs, RV::I64(x)) | (ScalarType::TimestampUs, RV::TimestampUs(x)) => {
            ScalarOut::TimestampUs(x)
        }
        (_, RV::U8(x)) => ScalarOut::U8(x),
        (_, RV::U16(x)) => ScalarOut::U16(x),
        (_, RV::U32(x)) => ScalarOut::U32(x),
        (_, RV::U64(x)) => ScalarOut::U64(x),
        (_, RV::I8(x)) => ScalarOut::I8(x),
        (_, RV::I16(x)) => ScalarOut::I16(x),
        (_, RV::I32(x)) => ScalarOut::I32(x),
        (_, RV::I64(x)) => ScalarOut::I64(x),
        (_, RV::F32(x)) => ScalarOut::F32(x),
        (_, RV::F64(x)) => ScalarOut::F64(x),
        (_, RV::Bool(b)) => ScalarOut::Bool(b),
        (_, RV::TimestampUs(x)) => ScalarOut::TimestampUs(x),
        (_, RV::Utf8(s)) => ScalarOut::Utf8(s),
        // Not built: no writer produces a list today. A reader that met one is looking at a
        // future format — absence, not a guess.
        (_, RV::List(_)) => return None,
    })
}

/// Every group-scoped attribute value one item carries that this session may see — one entry per
/// family, keyed by the owning group's key, values read from the per-view entity-space columns. A
/// scoped family has one column per view but the value belongs to the `(entity, key)` pair, so the
/// address is the key, resolved through [`owning_key_of`] from each visible view id to the key it
/// holds in this family's group.
/// The gate is two tests. First, the owning group's gate, for the whole family: a principal who
/// cannot reach it gets the attribute undeclared, and a leaf naming it takes the unknown-column
/// `422`. Not the per-view test: a group sharing the owner's roster carries its own gate, so a
/// principal failing the owner's gate but reaching a sharing group still passes every per-view
/// test below — the family's own gate is the only barrier left. Then, per key, the session must
/// reach a view that holds it; a family no reachable view holds is absent whole. Nothing here can
/// name a gate-failed view: every candidate key is tested against [`crate::gate::VisibleViews`]
/// before it is minted.
/// Every value read is this item's own, at an entity already established visible, indexed by
/// entity id. Does not serve a `text` family, which has no per-entity value slot. A value written
/// by a flush for a family with neither `index` nor `render` serves the build's values and
/// nothing since; every other family is served live.
fn scoped_values_of(
    generation: &Generation,
    visible: &crate::gate::VisibleViews,
    entity: u32,
) -> Vec<ItemScoped> {
    let manifest = &generation.bundle.manifest;
    let members_of = |name: &str| {
        manifest
            .groups
            .iter()
            .find(|g| g.name == name)?
            .members_of
            .as_deref()
    };
    // Every reachable view, with its roster key, built once for all families.
    let reachable: Vec<(&str, &str)> = manifest
        .groups
        .iter()
        .flat_map(|g| {
            g.views
                .iter()
                .map(move |v| (g.name.as_str(), v.key.as_str()))
        })
        .filter(|(group, key)| {
            visible.contains_view(&format!("{group}{}{key}", tessera_store::GROUP_SEPARATOR))
        })
        .collect();

    let mut out: Vec<ItemScoped> = Vec::new();
    for family in manifest.groups.iter().flat_map(|g| g.scoped_scalars.iter()) {
        // The owning group's gate, before anything about a view; see this function's doc.
        if !visible.contains_group(&family.group) {
            continue;
        }
        if !family.has_value_column() {
            continue;
        }
        // Sorted and deduplicated by key: two views sharing one key are one value.
        let mut values: std::collections::BTreeMap<String, ScalarOut> =
            std::collections::BTreeMap::new();
        for &(group, key) in &reachable {
            let Some(owned) = owning_key_of((group, key), members_of, &family.group) else {
                continue;
            };
            let id = format!("{}{}{owned}", family.group, tessera_store::GROUP_SEPARATOR);
            // The family's own list, not the roster: a view created since the build has no column
            // until one is written for it.
            if !family.views.contains(&id) {
                continue;
            }
            let column = crate::filter::scoped_column_name(&family.name, &id);
            let Some(stored) = generation.filter_columns.stored_value(&column, entity) else {
                continue;
            };
            if let Some(value) = stored_field_out(
                stored,
                family.arrow_type,
                family.vocabulary.as_deref(),
                &generation.vocabularies,
            ) {
                values.insert(owned.to_string(), value);
            }
        }
        if values.is_empty() {
            continue;
        }
        out.push(ItemScoped {
            name: family.name.clone(),
            values: values.into_iter().collect(),
        });
    }
    out.sort_by(|a, b| a.name.cmp(&b.name));
    out
}

/// A category code's drill-down value: its vocabulary key. Code 0 — the reserved absent
/// sentinel — is absence, and a code no binding explains is omitted rather than served raw.
fn category_key_out(
    code: u32,
    vocabulary: Option<&str>,
    vocabularies: &Vocabularies,
) -> Option<ScalarOut> {
    if code == 0 {
        return None;
    }
    let vocabulary = vocabularies.get(vocabulary?)?;
    let (key, _) = vocabulary.bindings().find(|&(_, c)| c == code)?;
    Some(ScalarOut::Utf8(key.to_string()))
}
