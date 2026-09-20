//! The item drill-down: one visible item's record, the views it sits in and its scoped values.

use super::*;
use super::out::flat_families;

/// `POST /v1/items/{handle}`'s payload (R5): a visible item's full record — every declared field
/// that carries a value, by **declared name** — plus its caller-supplied external id, if it has
/// one.
///
/// Names, not tags, and nothing else (I10): a blob field's tag is a declaration position and an
/// index internal, resolved to the declared name engine-side; no tag, no entity id and no blob
/// addressing detail crosses the trust boundary. A category field carries its vocabulary **key**,
/// never its code-as-value ambiguity — the code is what the hot path ships, and the drill-down is
/// precisely the surface that resolves it.
#[derive(Debug, Clone, PartialEq)]
pub struct ItemOut {
    /// Present fields only, in declaration order — an absent field is absent, not null, which is
    /// the same statement the record blob makes byte-wise (records §3).
    pub fields: Vec<ItemField>,
    pub external_id: Option<Vec<u8>>,
    /// **The satisfied terms only** (decision 0114): the intersection of this item's own term set
    /// with the asking session's satisfied set, presented through the plugin, sorted bytewise.
    ///
    /// Never the item's full label set. A viewer learning a compartment they do not hold is the
    /// disclosure this endpoint would otherwise be, and the intersection is what makes every
    /// string here computable from inside the principal's own authority (**I2**). It is taken
    /// against [`Session::satisfied_descriptors`], which holds only the descriptors the
    /// credential itself presented — so a term outside the grant has no name to be served under,
    /// whatever the intersection does.
    ///
    /// **Sorted by the presented string, not by term ordinal.** Ordinal order is the corpus's
    /// interning order, which is a fact about the whole dictionary rather than about this
    /// principal; sorting the strings is deterministic and says nothing the set does not.
    pub labels: Vec<String>,
    /// **The views this item holds a row in that this session may reach**, sorted by id, each with
    /// the position that view places it at (owner ruling 2026-09-01).
    ///
    /// A view the gate refuses is absent, exactly as a view nobody declared is (`views.md` §6): the
    /// array is built from the session's own [`crate::Session::visible_views`], so it can never
    /// become the one place a gate-failed view is named. Empty is therefore two different facts
    /// wearing one shape — an item held only in views this principal cannot reach, and an item in
    /// no view at all — and that is deliberate: distinguishing them is precisely the disclosure
    /// the gate exists to prevent. (An item in no view at all is a `404` before this is built, so
    /// what a client actually sees is the first case alone.)
    ///
    /// **A position is a fact about a view, not about an item.** Two views of one bundle quantise
    /// against different frames and may be projected differently (decision 0040), so the same item
    /// sits at a different `(x, y)` in each and there is no bundle-wide position to serve instead.
    pub views: Vec<ItemView>,
    /// **The group-scoped attribute values this principal may see** (`views.md` §5, owner ruling
    /// 2026-09-01), one entry per family, sorted by family name.
    ///
    /// **Keyed by the group's key, because the key is a view's only address**
    /// ([decision 0113](../../../docs/decisions/0113-ordinals-are-removed-and-the-key-is-the-only-address.md)):
    /// a family's value belongs to a `(entity, key)` pair, and two views sharing a key through a
    /// `members` group share the value. Gate-filtered per key on the same set `views` is: a key
    /// whose views this principal cannot reach is absent, and a family with no reachable key is
    /// absent whole.
    ///
    /// **Every family with a per-view value column, whatever its flags** — which is what a
    /// declaration with neither `index` nor `render` means: stored, served here, on no filter
    /// surface and in no row tail. A `text` family is the one absent kind, having no per-entity
    /// value slot to read (`views.md` §5).
    pub scoped: Vec<ItemScoped>,
}

/// One view a drill-down's item holds a row in, and where that view puts it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ItemView {
    /// The view's id — a plain view's name or `<group>:<key>`, the same address `/v1/meta` and
    /// every viewer verb use.
    pub id: String,
    /// The horizontal axis in **this view's own grid units**, 32-bit fixed point against the frame
    /// `/v1/meta` publishes for this view. The same units the viewport's Morton codes decode to,
    /// deinterleaved server-side because a drill-down carries one point and a JSON number cannot
    /// hold a 64-bit code exactly.
    pub x: u32,
    /// The vertical axis, on [`Self::x`]'s terms.
    pub y: u32,
}

/// One group-scoped attribute family's values for a drill-down's item, keyed by the group's key.
///
/// **No group name here**, though the keys are one group's: `/v1/meta`'s `scoped_scalars` already
/// says which group each family is scoped to, and a second copy beside the values is a second
/// thing to disagree with the first.
#[derive(Debug, Clone, PartialEq)]
pub struct ItemScoped {
    /// The family's name, as a filter leaf spells it before any pin.
    pub name: String,
    /// The values this item carries, by key, sorted by key. A key the item carries no value under
    /// is absent rather than null, the same rule the record's fields follow; a key this principal
    /// cannot reach is absent for a different reason, and the two are deliberately one shape.
    pub values: Vec<(String, ScalarOut)>,
}

/// One declared field of a drill-down record: the column's declared name and its value.
#[derive(Debug, Clone, PartialEq)]
pub struct ItemField {
    pub name: String,
    pub value: ScalarOut,
}

impl Engine {
    /// Is `entity` visible to `session` under `generation` — the ONE BIT `/v1/items` needs. An
    /// **entity-space** question (see `crate::compose::visible_to`'s doc): three constant-time
    /// probes, no `RowProjection` constructed or consulted, so this costs the same whether
    /// `entity` exists and is visible, exists and is not, or does not exist at all (Critical
    /// C-5, closed rather than narrowed).
    ///
    /// **Takes the fragment as an argument.** A session's own fragment goes stale at every flush —
    /// see [`Engine::fragment_for`] — and a drill-down that answered from the stale one would
    /// report a flushed item as invisible while the viewport beside it drew the mark. The caller
    /// has already resolved the generation once (lifecycle §1.1) and brings the fragment forward
    /// against that same snapshot.
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
    /// satisfied set, presented through the plugin (decision 0114).
    ///
    /// **Satisfied-only, twice over.** The intersection is `filter_map` over the entity's stored
    /// ordinals against [`Session::satisfied_descriptors`]; that map holds exactly the descriptors
    /// the credential presented, plus `public`, so there is no descriptor in scope for a term
    /// outside the grant even if the intersection were written wrongly. Nothing here reads the
    /// bundle dictionary, and there is deliberately no route from an ordinal to a descriptor that
    /// does not pass through the session.
    ///
    /// **Reached only after the visibility verdict**, like every other read in [`Engine::item`]:
    /// this is called from inside the row-bearing arm, so the transpose is never probed for an
    /// entity the principal cannot see and C4 stays closed by position rather than by measure.
    ///
    /// An entity the transpose does not hold answers `[]` rather than refusing. That is the
    /// direction that hides a label rather than inventing one, and it is reachable only while a
    /// prefix predates the transpose — the base layer covers every entity a build knew and each
    /// flush publishes its own.
    ///
    /// ⊘ **The plugin routing is the built-in one.** There is no wasmtime host (design §6.1's
    /// standing gap), so `present_terms` is answered by `builtin:passthrough`, whose descriptors
    /// are the caller's own label strings and whose presentation is therefore the identity. When a
    /// host arrives this call site does not change; the plugin behind `self.plugin` does.
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
        // **One string per descriptor, and the count is the enforcement of it.** A plugin
        // returning MORE strings than it was handed would put on the wire strings that answer to
        // no term this session satisfies — which is exactly the disclosure C30's structural claim
        // rules out, arriving through the one function the claim does not itself constrain. Fewer
        // is a lost label rather than an invented one, and is refused with it: positional is the
        // contract, so a short list means the caller cannot say which label it failed to present.
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

    /// `POST /v1/items/{handle}` (R5): validate `idset` if the caller sent one, invert `id` to
    /// its entity, test visibility in entity space, and only then locate a row and read its
    /// scalars/external id.
    ///
    /// **`idset` is checked against the SAME generation this call loads for the lookup below —
    /// never a separate `Engine::meta()` call.** A handler that called `Engine::meta()` (its own
    /// `generation.load_full()`, plus a clone of every declared scalar and view name, just to read
    /// one field) before calling this method would load the generation twice for one logical
    /// request, against lifecycle §1.1's one-load-per-request invariant. Checking here, first,
    /// against the snapshot already in hand is not merely cheaper: it closes the gap where a
    /// generation swap landing between the two calls validates the idset against one
    /// generation and serves the lookup from another.
    ///
    /// Returns `Ok(None)` both when `id` names nothing in this bundle and when it names an item
    /// the principal may not see — deliberately one outcome from one code path, so the server
    /// cannot differentiate what the engine does not tell it (contracts §3.2).
    ///
    /// **The timing channel is closed, not narrowed** (design Appendix C, C4
    /// annotation). The idset check is entity-independent — it runs identically for every `id`,
    /// before inversion, and does not read `id` at all — so it opens no channel of its own.
    /// Inversion is a pure function taking no I/O. The visibility test that follows is an
    /// entity-space question — three constant-time probes — and is **the same three probes for
    /// an identifier that names nothing and one that names an invisible item**. No
    /// `RowProjection` is constructed or read, so there is no per-ID cost for an attacker to
    /// correlate against, warm or cold. A row is located only after the answer is already
    /// "visible", and the sidecar is read only after that.
    ///
    /// **Returns `Err` rather than a fail-open `None`.** A digest mismatch, an
    /// out-of-order extent or a short locator is a `500`, never an item served with
    /// `external_id: null` — `.ok().flatten()` would discard exactly the typed errors the sidecar
    /// exists to produce. This does not reopen the timing channel: the sidecar is touched only for
    /// an item already established as visible, so no attacker-drivable path can raise it.
    pub fn item(
        &self,
        session: &Session,
        id: TesseraId,
        idset: Option<u32>,
    ) -> Result<Option<ItemOut>> {
        let generation = self.generation.load_full();

        // Checked FIRST, against the generation this call already loaded above — see this
        // method's doc for why that (not a separate `Engine::meta()` call) is load-bearing here.
        if let Some(e) = idset {
            if e != generation.bundle.manifest.identity.idset {
                return Err(EngineError::StaleIdSet);
            }
        }

        let (shard, entity) = self.identity_key.invert(id);
        if shard != generation.bundle.manifest.identity.shard_id {
            return Ok(None);
        }

        // Brought forward before the visibility test, not after: the whole point of the test is
        // that it is the same three probes for every identifier (C-5), and a fragment resolved
        // per-entity would make the cost depend on which entity was asked for.
        //
        // **The served fragment, not the live one** (decision 0044). Rebuilding at the live
        // watermark is a *measured* ~200 ms per credential per publication on this thread, and it
        // would answer from a different watermark than the viewport beside it serves from — the
        // two enforcement representations drifting under stale-serve. Falls back to a build only
        // when this session has no resident entry at all, which is establishment. **No projection
        // is constructed either way**: this is a read of the cache, never a claim on it.
        // **Scoped to this generation's prefix**, so a fold's flip cannot answer an
        // entity-space question from a fragment built against the term index it replaced — see
        // `RowProjectionCache::freshest_fragment`.
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

        // Visible. Now — and only now — find the row, so the cost below is never reachable by
        // an identifier the principal may not see.
        //
        // **Every read below sits strictly after the visibility verdict, so C4 stays closed by
        // construction, not by measure.** The verdict above is the same three constant-time
        // entity-space probes for an identifier that names nothing and one that names an
        // invisible item; the row lookup, the entity-space value reads, the blob block read and
        // the sidecar read are all reachable only for an item already established visible —
        // exactly the position the external-id sidecar has always occupied. The blob's block
        // decompression is therefore not a probe-able cost: no attacker-drivable path reaches it
        // for an item the principal cannot see (X1's surface, bounded the same way the sidecar's
        // is).
        //
        // The allocator caps entity ids at `u32::MAX` (I9), and inversion produced this one from
        // a 32-bit half; checked rather than cast so a violated invariant fails loudly.
        let entity_raw =
            u32::try_from(entity.raw()).expect("entity ids are capped at u32::MAX by I9");
        let rows = rows_of(&generation, entity)?;
        let views = item_views(&rows, session.visible_views());
        let Some(&(_view, segment, local)) = row_to_read(&rows, session.visible_views()) else {
            // Visible in entity space but with no row anywhere: a buffered item awaiting flush.
            // Same `Ok(None)`, same 404 — it has no geometry to return.
            return Ok(None);
        };

        // **The scoped values, gate-filtered by the same set and keyed by the group's key** — the
        // key being a view's only address (decision 0113). See [`scoped_values_of`].
        let scoped = scoped_values_of(&generation, session.visible_views(), entity_raw);
        let fields = record_fields(&generation, segment, local, entity_raw)?;
        Ok(Some(ItemOut {
            fields,
            labels: self.labels_for(&generation, session, entity_raw)?,
            views,
            scoped,
            // N-3: propagate, never swallow. `EngineError::Store`, the same wrapping
            // every other store-backed call in this crate uses (see `Engine::open`).
            // Against the generation this request loaded, never a second `load()`: the
            // sidecar is per-generation now, and a fold rewrites it.
            external_id: self
                .external_id_of_in(&generation, entity)
                .map_err(EngineError::Store)?,
        }))
    }
}

/// **Every view this item holds a row in, resolved in one pass**, sorted by view id.
///
/// The `views` array is the gate-filtered part of this list and the record is assembled from one
/// row of it, so a second walk would be a second chance to disagree about which rows exist.
///
/// **Proportionate for one point**: the permutation is the only entity→row bridge (I4,
/// §5.1) — an O(1) bounds-checked slot read per view, not a scan — and a view holds more
/// than one segment once anything has flushed, so the *view*-space row must be resolved to
/// the segment that owns it and to that segment's local index before anything is read
/// ([`segment_row_of`], which is that resolution's one definition). A position is then two
/// indexed reads and a bit permutation. So the whole `views` array costs O(views), with no
/// per-view file read at all, and membership is never served without its position.
///
/// **Sorted, because the maps walked here are hash maps.** Both the partitions and a partition's
/// views iterate in an arbitrary order, so without this the record's home view — and the
/// `views` array's order — would differ between two identical requests to one process.
///
/// **One entry per view id, without deduplicating for it.** A view id is a key of one
/// partition's map, and an entity lives in exactly one partition (I5 splits entity space),
/// so `segment_row_of` can answer for at most one partition and no id can appear twice. The
/// sort is therefore a total order on distinct ids rather than a grouping, and `views` is a
/// set. A partitioning that put one entity in two partitions would break that here as it
/// would break every other entity-space read.
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

/// **The positions, gate-filtered** (`views.md` §6, owner ruling 2026-09-01): one entry per
/// view of this item's that the session may reach, and nothing at all for the views it may
/// not. A view failing the gate is absent exactly as a view nobody declared is, so the
/// array never becomes the one place a gate-failed view is named.
///
/// The position is the view's own grid units — the 64-bit interleave the row stores split
/// across `morton.u32` and the residual column, deinterleaved through the inverse of what
/// wrote it. It decodes against the frame `/v1/meta` publishes **for that view** and no
/// other (decision 0040), which is the whole reason a per-view position is a different
/// quantity per view rather than one position repeated.
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

/// **The row the record's homes read: one of a view this principal may reach, where one
/// exists.** Home 1 is a row read, and the rows are ordered by view id — so without this
/// the field values would come from whichever view sorts first, a gate-failed one included,
/// and a sealed view named `a…` would supply the record every principal is served.
///
/// Nothing is disclosed either way: home 1 reads the *declared* render scalars, which are
/// entity-scoped and hold the same value in every view (a scoped family has no slot in
/// `declared_scalars`). What the choice buys is that the served record is a fact about a
/// view the principal knows exists, so nothing about the answer traces back to a view they
/// may not reach. The fallback is deliberate rather than a fail-closed refusal: a point
/// held only in views this principal cannot reach is served today and stays served
/// (`ItemOut::views`), and its record is what it always was.
fn row_to_read<'a>(
    rows: &'a [(&str, &SegmentData, usize)],
    visible: &crate::gate::VisibleViews,
) -> Option<&'a (&'a str, &'a SegmentData, usize)> {
    rows.iter()
        .find(|(view, _, _)| visible.contains_view(view))
        .or_else(|| rows.first())
}

/// **The record, assembled from its three homes** (records §3): render fields from the row's
/// scalar tail, indexed and category fields from their entity-space structures — a category's
/// code resolved to its vocabulary key — and everything else from one record blob read. Field
/// identity is the declared *name*, resolved engine-side from the blob's positional tag; no tag,
/// no entity id and no blob internal reaches the wire (I10).
///
/// Reached only after [`Engine::item`]'s visibility verdict, like every other read there.
fn record_fields(
    generation: &Generation,
    segment: &SegmentData,
    local: usize,
    entity: u32,
) -> Result<Vec<ItemField>> {
    let manifest = &generation.bundle.manifest;
    // **Render columns only in the row read.** The compiled schema includes entity-space and
    // blob-resident columns, which are absent from `columns.arrow` by design; those are
    // homes 2 and 3 below, never a column of nulls under a name a client can see.
    let render_scalars: Vec<_> = manifest.render_scalars().cloned().collect();

    // One value slot per declared column, filled home by home; a column no home
    // holds a value in stays `None` and is omitted — absence is absence.
    let mut values: Vec<Option<ScalarOut>> = vec![None; manifest.declared_scalars.len()];

    // Home 1: the row. The same `resolve_scalars` the viewport gather uses, so the
    // two read paths cannot disagree about what a stored type decodes to.
    let resolved = resolve_scalars(segment, &render_scalars);
    for (slot, declared_index) in manifest.render_indices().enumerate() {
        let Some(view) = &resolved[slot] else {
            continue;
        };
        let d = &manifest.declared_scalars[declared_index];
        // **Absence is the presence bitmap beside the column, never a zero in it** (decision
        // 0064), on `flushed_row_scalar`'s rule: a row whose slot the writer marked absent
        // holds the type's zero as a placeholder and carries no value. A category needs no
        // bitmap: its absence is the reserved code, which `row_field_out` reads as none.
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

    // Home 2: entity space — every non-rendered column with a value column (indexed
    // columns, and the per-viewer vocabulary floor), at drill-down cadence.
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

    // Home 3: the record blob — one block read, strictly after the verdict (see
    // [`Engine::item`]'s doc). Fail-closed: a malformed row, a tag past the schema or an
    // addressing defect refuses the request rather than serving a neighbour's field
    // under this item's identity (records §3, review B6).
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
/// key — code 0, the absent sentinel, resolving to absence — and every other family as stored.
///
/// A rendered *number*'s absence is still stored as the type's zero (`or_render_placeholder`;
/// 0064's wire half being deferred), so a numeric zero here may be a real zero or an absence —
/// the row cannot say which, and this reports the stored value rather than inventing a rule. The
/// entity-space and blob homes do not share the ambiguity.
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
            // A category is one of the three widths; anything else is a malformed tail the
            // gather refuses on its own path. Absence is the honest answer here.
            _ => return None,
        };
        return category_key_out(code, d.vocabulary.as_deref(), vocabularies);
    }
    // Generated for the flat members; `Bool` and `Utf8` read through their arrays because
    // neither is stored as a flat slice of itself.
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
/// says the slot is empty — the join rule's attribute arm, home 1 (`views.md` §4, records §6.2).
///
/// **This home exists because a rendered column need not have an entity-space one.** `render =
/// true, index = false` over a non-`derived` vocabulary owes no value column and is not
/// blob-resident, so the hot column is the value's *only* store; an oracle reading the other two
/// homes alone would report "no value held" for the most ordinary attribute declaration there is,
/// and accept every mismatch against it.
///
/// **Every view is scanned, and a view holding no value is skipped rather than answering.** The
/// first version stopped at the first view whose permutation held a row and returned `None` if
/// *that* row's presence bit was clear — first-view-wins over a `HashMap` of partitions and views,
/// so an entity holding a value in one view and an absence in another answered `200` or `409` by
/// hash order (r24 review F1). Views can hold different tails lawfully: a join whose batch omitted
/// a render-home value writes an absent slot, and until the backfill below fills it that view is a
/// genuine absence beside another view's value. Absence is the *weaker* answer — the comparison
/// reads it as "nothing held", which accepts — so it must never pre-empt a view that holds
/// something. `None` here means no view holds a present value, which is the only reading of it
/// the caller is entitled to.
///
/// **A malformed segment set skips that view too**, and does not abandon the scan — the drill-down
/// skips a view it cannot resolve for the same reason. Where *no* view answers, the caller gets
/// `None` and the join is accepted unchecked, which is the posture [`Engine::flushed_terms`] takes
/// for the label arm: the join changes nothing in entity space either way, so a corrupt artefact
/// loses the *report* rather than turning a caller's batch into a server error. The warning names
/// the view and never the entity (**I10**: the byte-scanner sweeps logs as well as payloads).
pub(crate) fn flushed_row_scalar(
    generation: &Generation,
    entity: EntityId,
    declared_index: usize,
) -> Option<tessera_filter::RecordValue> {
    use tessera_filter::RecordValue as RV;

    let manifest = &generation.bundle.manifest;
    // The slot this declared column occupies in the *render* tail, which is the only tail a
    // segment carries. A column that is not rendered has no hot-column home at all.
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
            // **Absence is the presence bitmap beside the column, never a zero in it** (decision
            // 0064). A category needs no bitmap and has none: its absence is the reserved code, in
            // band, which the comparison reads as absence on both sides.
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
/// a `bool`'s `u8` storage back to `bool`, a `timestamp_us`'s `i64` back to its unit.
///
/// **Over the two facts rather than over the declaration**, because a group-scoped family is not
/// one of `declared_scalars` and has no [`DeclaredScalar`] to pass: the storage type and the
/// vocabulary it names are the whole of what this needs, and both records carry them.
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
        // ⊘ Lists land with epic 3's multi surface; no writer produces one today, and a reader
        // that met one would be looking at a future format — absence, not a guess.
        (_, RV::List(_)) => return None,
    })
}

/// **Every group-scoped attribute value one item carries that this session may see**
/// (`views.md` §5, owner ruling 2026-09-01) — one entry per family, keyed by the owning group's
/// key, values read from the per-view entity-space columns.
///
/// # Why the key and not the view
///
/// A scoped family has one column per view, but the value belongs to the `(entity, key)` pair:
/// two groups sharing a roster through `members` share the column, and a client that read the
/// same value twice under two view ids would be reading one fact as two. So the address is the
/// key ([decision 0113](../../../docs/decisions/0113-ordinals-are-removed-and-the-key-is-the-only-address.md)),
/// resolved through [`owning_key_of`] — §3.3's ownership rule, stated once — from each visible
/// view id to the key it holds in *this family's* group.
///
/// # The gate, which is two tests and not one
///
/// **The owning group's gate first, and it is the whole family's** (`views.md` §5, §6). A family
/// belongs to the group that owns the views it has a column per, and for a principal who cannot
/// reach that group the *whole attribute is undeclared*: `/v1/meta` omits it from both
/// `scoped_scalars` and `filter_operands`, and a leaf naming it takes the unknown-column `422`
/// that confirms neither group nor key. Serving its name and its values here would be the one
/// surface that told them otherwise.
///
/// **It is reachable and it is not the per-view test.** A group declaring `members` of the owner
/// carries its own gate, and nothing requires the two to agree — a sealed owner shared under a
/// public roster is the ordinary way to publish a second layout of someone else's quarters. A
/// principal failing the owner's gate then reaches the *sharer's* views, `owning_key_of` resolves
/// each to the owner's key, and every per-view test below passes. The family's own gate is the
/// only thing standing between that principal and a sealed group's attribute.
///
/// **Then the per-view test, per key.** A key is served only where the session may reach a view
/// that holds it, its own group's or a sharing group's; a family no reachable view holds is absent
/// whole rather than served empty. Nothing here can name a gate-failed view: the enumeration
/// starts from the roster and every candidate is tested against [`crate::gate::VisibleViews`]
/// before its key is minted, so the answer is a function of the views this principal already knows
/// about.
///
/// # I2
///
/// Every value read is this **item's own**, at an entity the caller has already established
/// visible, out of an entity-space column indexed by entity id. There is no aggregate here and no
/// quantity derived from anything outside `M_auth`: the register's argument is the one C30 makes
/// for the labels beside it.
///
/// # What it does not serve
///
/// A `text` family, which has no per-entity value slot — its entity-space artefacts are a token
/// dictionary and the postings over it, so there is nothing to read for one entity
/// ([`tessera_store::manifest::ScopedScalar::has_value_column`]).
///
/// ⊘ A value written by a **flush** for a family with neither `index` nor `render`: the flush
/// writes no extent for one (`Engine::flush`'s scoped pass, on `filter::scoped_is_filterable`), so
/// such a family serves the build's values and nothing since. Every other family — indexed,
/// rendered, or both — takes its extents and is served live.
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
    // Every view of every group this session may reach, with the roster record that decides which
    // key it holds where. Built once for all the families rather than per family: a corpus with
    // eight families over one group of forty quarters would otherwise walk the roster eight times.
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
        // **The owning group's gate, before anything about a view** — the test every other scoped
        // surface makes (`EngineMeta::resolve_filter_column`, `/v1/meta`'s two lists,
        // `scoped_render_families`), and see this function's doc for the shape that reaches it: a
        // sealed owner shared under a public `members` roster passes every per-view test below.
        if !visible.contains_group(&family.group) {
            continue;
        }
        if !family.has_value_column() {
            continue;
        }
        // Sorted and deduplicated by key: two views sharing one key through a `members` group are
        // one value, and a client reading the map has no order of its own to fall back on.
        let mut values: std::collections::BTreeMap<String, ScalarOut> =
            std::collections::BTreeMap::new();
        for &(group, key) in &reachable {
            let Some(owned) = owning_key_of((group, key), members_of, &family.group) else {
                continue;
            };
            let id = format!("{}{}{owned}", family.group, tessera_store::GROUP_SEPARATOR);
            // The family's own list, not the roster: a view created since the build has no column
            // until one is written for it, and asking for one would be asking for a file no pass
            // wrote.
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

/// A category code's drill-down value: its vocabulary **key**. Code 0 — the reserved absent
/// sentinel — is absence, and a code no binding explains is omitted rather than served raw,
/// the same rule `/v1/categories` applies to an unresolvable code.
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
