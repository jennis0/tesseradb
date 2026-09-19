use super::*;

/// What one values batch's fill rule produced: the cells to hold until the flush writes them, and
/// the counts the acknowledgement carries.
pub(super) struct PlannedFills {
    /// The entity-scoped cells, one entry per entity.
    pub(super) fills: Vec<(EntityId, tessera_lifecycle::Fill)>,
    /// The group-scoped cells, one entry per `(entity, owner view)`. A scoped value's address is
    /// the pair, never the entity alone.
    pub(super) scoped_fills: Vec<(EntityId, String, tessera_lifecycle::ScopedFill)>,
    pub(super) filled: u64,
    pub(super) held: u64,
}

/// Where one of a values batch's named columns lands in a row's two positional spaces.
pub(super) enum ValuesColumn {
    /// A position in `MANIFEST.declared_scalars`, the space a buffered row's `scalars` is
    /// positional against.
    Entity(usize),
    /// A position in the view's group-scoped families, the space its `scoped` list is positional
    /// against.
    Scoped(usize),
}

/// The key half of an owner view id: the half a caller spelled, never the owning group. A sharing
/// group's caller has no business learning the owning group from a refusal.
pub(super) fn key_of_owner_view(owner_view: &str) -> &str {
    owner_view
        .split_once(tessera_store::GROUP_SEPARATOR)
        .map_or(owner_view, |(_, key)| key)
}

/// What this deployment already holds for one entity and one entity-scoped column: the entity's
/// own buffered row, then `pending`, then the flushed homes.
///
/// Each source is asked for a held value, not for a slot: a source holding the column's absence
/// falls through to the next, so an absent cell in one source is not read as unheld while another
/// holds a value for it.
pub(super) fn held_entity_value(
    generation: &Generation,
    entity: EntityId,
    at: usize,
    declared: &tessera_store::manifest::DeclaredScalar,
    buffered: Option<&tessera_lifecycle::BufferedItem>,
    pending: Option<&tessera_lifecycle::Fill>,
    blob: &mut crate::session::BlobRow,
) -> Option<WalScalar> {
    let held =
        |value: WalScalar| (!crate::session::scalar_is_absent(&value, declared)).then_some(value);
    buffered
        .and_then(|item| item.scalars.get(at).cloned())
        .and_then(held)
        .or_else(|| {
            pending
                .and_then(|fill| fill.scalars.get(at).cloned())
                .and_then(held)
        })
        .or_else(|| crate::session::flushed_scalar_of(generation, entity, at, blob).and_then(held))
}

/// What this deployment already holds for one `(entity, attribute, key)` cell, on
/// [`held_entity_value`]'s rule for absence.
///
/// The buffered source is every row of the entity whose view addresses this same key: the cell's
/// own rows, not the entity's own row, which is a different question.
pub(super) fn held_scoped_value(
    generation: &Generation,
    entity: EntityId,
    at: usize,
    family: &tessera_store::manifest::ScopedScalar,
    declared: &tessera_store::manifest::DeclaredScalar,
    owner_view: &str,
    pending: Option<&tessera_lifecycle::ScopedFill>,
) -> Option<WalScalar> {
    let manifest = &generation.bundle.manifest;
    let held =
        |value: WalScalar| (!crate::session::scalar_is_absent(&value, declared)).then_some(value);
    generation
        .buffer
        .rows_of(entity)
        .filter(|item| scoped_owner_view_of(manifest, &item.view) == owner_view)
        .find_map(|item| item.scoped.get(at).cloned().and_then(held))
        .or_else(|| {
            pending
                .and_then(|fill| fill.scoped.get(at).cloned())
                .and_then(held)
        })
        .or_else(|| {
            crate::session::flushed_scoped_of(generation, entity, family, owner_view).and_then(held)
        })
}

/// Apply the fill rule to one values batch: the cells nothing holds, refusing on the first cell
/// held differently. Claims each cell from three sources in order: the buffered row, an earlier
/// batch's unflushed fill, and the flushed homes. An absent cell has no claimant in any of them,
/// so extents stay disjoint per column when the flush writes them.
///
/// A row index, a column name and a key reach the caller; nothing else does (I10).
pub(super) fn plan_fills(
    generation: &Generation,
    request: &tessera_lifecycle::ValuesRequest,
) -> Result<PlannedFills, ExecError> {
    let manifest = &generation.bundle.manifest;
    let declared = &manifest.declared_scalars;
    let families = scoped_families_of_view(manifest, &request.view);
    let owner_view = scoped_owner_view_of(manifest, &request.view);
    let key = key_of_owner_view(&owner_view);

    // One resolution per batch, not per row: a name in neither space is refused here too, since
    // the door reads a generation this pass may have moved past.
    //
    // A `render` column cannot be filled: a fill acquires no row, so the value never reaches the
    // hot column a tile or drill-down reads from, and where the column is not also `index` there
    // is no other home either.
    let mut columns = Vec::with_capacity(request.columns.len());
    for name in &request.columns {
        if let Some(position) = declared.iter().position(|d| &d.name == name) {
            if declared[position].render {
                return Err(ExecError::ValuesRefused {
                    detail: format!(
                        "column '{name}' is declared `render` and a values row acquires no row \
                         for the value to be drawn from; re-ingest the point, or declare the \
                         column without `render`"
                    ),
                });
            }
            columns.push(ValuesColumn::Entity(position));
            continue;
        }
        if let Some(position) = families.iter().position(|f| &f.name == name) {
            if families[position].render {
                return Err(ExecError::ValuesRefused {
                    detail: format!(
                        "group-scoped column '{name}' is declared `render` and a values row \
                         acquires no row for the value to be drawn from; re-ingest the point, or \
                         declare the column without `render`"
                    ),
                });
            }
            columns.push(ValuesColumn::Scoped(position));
            continue;
        }
        return Err(ExecError::ValuesRefused {
            detail: format!(
                "column '{name}' is neither a declared scalar nor a group-scoped family whose key \
                 set holds view '{}'; declare the column, or name the view whose key addresses \
                 the cell",
                request.view
            ),
        });
    }

    let mut fills = Vec::with_capacity(request.rows.len());
    let mut scoped_fills = Vec::new();
    let mut filled = 0u64;
    let mut held_count = 0u64;
    for (index, row) in request.rows.iter().enumerate() {
        let entity = row.entity;
        if generation.overlay.is_deleted(entity) {
            return Err(ExecError::ValuesRefused {
                detail: format!(
                    "row {index} names an entity this deployment has deleted; re-ingest the item, \
                     which allocates a fresh entity"
                ),
            });
        }
        let buffered = generation.buffer.get(entity);
        // The cells an earlier batch filled and no flush has written. A lookup, not a scan: these
        // are asked once per row and a batch runs to `max_batch_rows`.
        let pending = generation.buffer.fill_of(entity);
        let pending_scoped = generation.buffer.scoped_fill_of(entity, &owner_view);
        // Read at most once for this row, and only if a blob-resident column asks.
        let mut blob = crate::session::BlobRow::default();
        // Absence in a fill's tails is `WalScalar::Null` for every family, category included: the
        // flush's gather maps `Null` onto the category's reserved code. Using `Null` here lets a
        // merge of two fills tell an unfilled cell from a filled one.
        let mut scalars: Vec<WalScalar> = vec![WalScalar::Null; declared.len()];
        let mut scoped: Vec<WalScalar> = vec![WalScalar::Null; families.len()];
        let mut any_entity = false;
        let mut any_scoped = false;

        for (position, column) in columns.iter().enumerate() {
            let Some(supplied) = row.values.get(position) else {
                continue;
            };
            match column {
                ValuesColumn::Entity(at) => {
                    let d = &declared[*at];
                    if crate::session::scalar_is_absent(supplied, d) {
                        continue;
                    }
                    let stored = held_entity_value(
                        generation,
                        entity,
                        *at,
                        d,
                        buffered,
                        pending,
                        &mut blob,
                    );
                    match stored {
                        None => {
                            scalars[*at] = supplied.clone();
                            filled += 1;
                            any_entity = true;
                        }
                        Some(stored) if stored == *supplied => held_count += 1,
                        Some(_) => {
                            return Err(ExecError::ValueConflict {
                                detail: format!(
                                    "row {index} supplies a different value for column '{}' than \
                                     this deployment already holds; an entity-scoped attribute is \
                                     one value per entity, so supply the value held or omit the \
                                     column",
                                    d.name
                                ),
                            })
                        }
                    }
                }
                ValuesColumn::Scoped(at) => {
                    let family = &families[*at];
                    let d = crate::session::declared_of_scoped(family);
                    if crate::session::scalar_is_absent(supplied, &d) {
                        continue;
                    }
                    let stored = held_scoped_value(
                        generation,
                        entity,
                        *at,
                        family,
                        &d,
                        &owner_view,
                        pending_scoped,
                    );
                    // A `text` family past a flush is refused rather than compared: the column
                    // stores a dictionary, postings and a presence bitmap, not a value per entity,
                    // so there is nothing to compare against. Occupancy is asked instead.
                    if stored.is_none()
                        && family.arrow_type == ScalarType::Text
                        && crate::session::flushed_scoped_text_present(
                            generation,
                            entity,
                            family,
                            &owner_view,
                        )
                    {
                        return Err(ExecError::ValueConflict {
                            detail: format!(
                                "row {index} supplies a value for group-scoped column '{}' and \
                                 this deployment already holds prose for key '{key}' that a flush \
                                 has made uncomparable; omit the column",
                                family.name
                            ),
                        });
                    }
                    match stored {
                        None => {
                            scoped[*at] = supplied.clone();
                            filled += 1;
                            any_scoped = true;
                        }
                        Some(stored) if stored == *supplied => held_count += 1,
                        Some(_) => {
                            return Err(ExecError::ValueConflict {
                                detail: format!(
                                    "row {index} supplies a different value for group-scoped \
                                     column '{}' than this deployment already holds for key \
                                     '{key}'; a scoped value is one value per cell, so supply the \
                                     value held or omit the column",
                                    family.name
                                ),
                            })
                        }
                    }
                }
            }
        }
        if any_entity {
            fills.push((
                entity,
                tessera_lifecycle::Fill {
                    view: request.view.clone(),
                    scalars,
                    wal_pos: None,
                },
            ));
        }
        if any_scoped {
            scoped_fills.push((
                entity,
                owner_view.clone(),
                tessera_lifecycle::ScopedFill {
                    view: request.view.clone(),
                    scoped,
                    wal_pos: None,
                },
            ));
        }
    }
    Ok(PlannedFills {
        fills,
        scoped_fills,
        filled,
        held: held_count,
    })
}

/// The growth records one values batch's layer columns produce.
///
/// A key with no ordinal was minted at this batch's own commit and the publication carried these
/// rows as its first members, so it is skipped here as [`growth_records`] skips one. Every other
/// reason a key could have no ordinal was refused before this, at
/// [`Executor::resolve_memberships`].
pub(super) fn values_growth_records(
    memberships: &[tessera_lifecycle::ResolvedMembership],
    rows: &[tessera_lifecycle::IncomingValues],
    store: &tessera_lifecycle::ArtifactStore,
) -> Result<Vec<WalRecord>, String> {
    use std::collections::BTreeMap;
    // Ordered, so the records a batch appends do not depend on hash iteration order: two nodes
    // replaying one log must read the same sequence.
    let mut by_level: BTreeMap<(&str, u32), BTreeMap<u32, croaring::Bitmap>> = BTreeMap::new();
    for join in memberships {
        let Some(ordinal) = join.ordinal else {
            continue;
        };
        let joining = by_level
            .entry((join.layer.as_str(), join.level))
            .or_default()
            .entry(ordinal)
            .or_default();
        for row in &join.rows {
            let Some(entity) = rows.get(*row as usize) else {
                return Err(format!(
                    "column '{}' names row {row}, which this batch does not carry",
                    join.layer
                ));
            };
            // Entity space is `u32`, so the narrowing is total.
            joining.add(entity.entity.raw() as u32);
        }
    }
    // What the artifact already holds is not a join: a page restating a membership the store
    // carries would otherwise append a record that changes nothing and pins the log at it, since
    // `growth_record` drops an empty set.
    //
    // Read against the store before the batch is applied, the only moment the difference exists:
    // [`Executor::commit_values`] holds the executor's one lock across the preparation.
    for ((layer, level), ordinals) in &mut by_level {
        for (ordinal, joining) in ordinals.iter_mut() {
            if let Some(record) = store.get(layer, *level, *ordinal) {
                joining.andnot_inplace(&record.members);
            }
        }
    }
    Ok(by_level
        .into_iter()
        .filter_map(|((layer, level), ordinals)| {
            tessera_lifecycle::membership::growth_record(
                layer,
                level,
                ordinals
                    .iter()
                    .map(|(ordinal, joining)| (*ordinal, joining)),
            )
        })
        .collect())
}

/// The artifacts one values batch's layer columns named and no artifact holds: [`mint_plan`]'s
/// twin for the values door.
///
/// An unknown key creates the artifact on an `open` layer, carrying the batch's own rows as its
/// first members. One artifact per key per level for the whole batch: two rows naming one unknown
/// key mint once and both join it.
pub(super) fn values_mint_plan(
    memberships: &[tessera_lifecycle::ResolvedMembership],
    rows: &[tessera_lifecycle::IncomingValues],
) -> Result<MintPlan, String> {
    let mut wanted: MintPlan = std::collections::BTreeMap::new();
    for join in memberships {
        if join.ordinal.is_some() {
            continue;
        }
        let (_, members) = wanted
            .entry((join.layer.clone(), join.level, join.key.clone()))
            .or_insert_with(|| (0, croaring::Bitmap::new()));
        for row in &join.rows {
            let Some(entity) = rows.get(*row as usize) else {
                return Err(format!(
                    "column '{}' names row {row}, which this batch does not carry",
                    join.layer
                ));
            };
            // Entity space is `u32`, so the narrowing is total.
            members.add(entity.entity.raw() as u32);
        }
    }
    Ok(wanted)
}

/// How many of one growth record's joining members the artifacts do not already hold, read before
/// the record is applied, which is the only time the difference exists.
pub(super) fn new_members_of(record: &WalRecord, store: &tessera_lifecycle::ArtifactStore) -> u64 {
    let WalRecord::ArtifactGrow {
        layer,
        level,
        growth,
    } = record
    else {
        return 0;
    };
    growth
        .iter()
        .map(|delta| {
            let Some(joining) = tessera_lifecycle::membership::deserialise_members(&delta.joining)
            else {
                return 0;
            };
            match store.get(layer, *level, delta.ordinal) {
                Some(record) => joining.andnot_cardinality(&record.members),
                None => 0,
            }
        })
        .sum()
}

/// Settle every joining row of one batch, whose join-ness `established_collisions` has just
/// decided: the **join rule**'s three arms (label, entity-scoped attribute, scoped cell), then the
/// completion an accepted join owes: descriptors dropped, omitted `render` values backfilled. A
/// refusal is the `409` the caller is answered with, whole batch without effect, taken before the
/// WAL append.
///
/// A row index and a column name reach the caller; nothing else does (I10).
pub(super) fn settle_joins(generation: &Generation, rows: &mut [UnallocatedRow]) -> Result<(), String> {
    if rows.iter().all(|row| row.join.is_none()) {
        return Ok(());
    }
    let manifest = &generation.bundle.manifest;
    let declared = &manifest.declared_scalars;
    // One derivation per batch, not per row: every row of a batch names one view, and this is the
    // list its `scoped` tail was parsed positionally against at the boundary.
    let view = rows.first().map(|row| row.view.as_str()).unwrap_or("");
    let scoped_families = scoped_families_of_view(manifest, view);
    let owner_view = scoped_owner_view_of(manifest, view);
    let key = key_of_owner_view(&owner_view);

    for (index, row) in rows.iter_mut().enumerate() {
        let Some(entity) = row.join else {
            continue;
        };
        let buffered = generation.buffer.get(entity);
        let pending = generation.buffer.fill_of(entity);
        let pending_scoped = generation.buffer.scoped_fill_of(entity, &owner_view);
        // Read at most once for this row, and only if a blob-resident column asks.
        let mut blob = crate::session::BlobRow::default();
        // The label arm reads the buffer first, the transpose after, and both are exact.
        //
        // A novel descriptor resolves to a process-local extension id no stored ordinal can equal,
        // so a batch naming a label the deployment has never interned is a mismatch: the flushed
        // entity cannot be carrying it.
        let held_terms: Option<Vec<u32>> = match &buffered {
            Some(buffered) => Some(buffered.terms.iter().map(|t| t.raw()).collect()),
            None => crate::session::flushed_terms_of(generation, entity)
                .map(|terms| terms.iter().map(|t| t.raw()).collect()),
        };
        if let Some(mut held_terms) = held_terms {
            let mut supplied_terms: Vec<u32> = row.terms.iter().map(|t| t.raw()).collect();
            supplied_terms.sort_unstable();
            supplied_terms.dedup();
            held_terms.sort_unstable();
            held_terms.dedup();
            if supplied_terms != held_terms {
                return Err(format!(
                    "row {index} joins an entity this deployment already holds, under a different \
                     access label; carry the label the entity holds, or delete the item and \
                     re-ingest it"
                ));
            }
        }
        // The attribute arm: an entity-scoped attribute is one value per entity, so a joining row
        // must carry the stored value or leave it absent, compared on values normalised to the
        // shape a batch carries (`stored_as_wal`).
        for (position, d) in declared.iter().enumerate() {
            let Some(supplied) = row.scalars.get(position) else {
                continue;
            };
            // An omitted value is not a disagreement, and is not written through as an absence
            // either: the backfill below fills a `render` column's omitted slot from the entity's
            // stored value, once join-ness is settled.
            if crate::session::scalar_is_absent(supplied, d) {
                continue;
            }
            let held = held_entity_value(generation, entity, position, d, buffered, pending, &mut blob);
            let Some(held) = held else {
                continue;
            };
            if held == *supplied {
                continue;
            }
            return Err(format!(
                "row {index} joins an entity this deployment already holds, with a different \
                 value for column '{}'; an entity-scoped attribute is one value per entity, so a \
                 joining row carries the stored value or omits the column",
                d.name
            ));
        }
        // The scoped cell arm: one value per `(entity, attribute, key)`, whichever door wrote it.
        // The cell a joining row addresses may already hold a value from the owning group's view
        // or any sharing group's. An empty cell takes the row's value, a matching cell drops the
        // row's copy, and a differing one refuses.
        //
        // A `text` family past a flush is refused rather than compared: the column stores a
        // dictionary, postings and a presence bitmap, not a value per entity, so there is nothing
        // to compare against. Occupancy is asked instead, and an occupied cell refuses regardless.
        for (position, family) in scoped_families.iter().enumerate() {
            let Some(supplied) = row.scoped.get(position) else {
                continue;
            };
            let d = crate::session::declared_of_scoped(family);
            if crate::session::scalar_is_absent(supplied, &d) {
                continue;
            }
            let held =
                held_scoped_value(
                generation,
                entity,
                position,
                family,
                &d,
                &owner_view,
                pending_scoped,
            );
            if held.is_none()
                && family.arrow_type == ScalarType::Text
                && crate::session::flushed_scoped_text_present(
                    generation,
                    entity,
                    family,
                    &owner_view,
                )
            {
                return Err(format!(
                    "row {index} names a value for group-scoped column '{}' and this deployment \
                     already holds prose for key '{key}' that a flush has made uncomparable; omit \
                     the column",
                    family.name
                ));
            }
            let Some(held) = held else {
                continue;
            };
            if held == row.scoped[position] {
                // The dedupe. Absence in this row's tail, and the cell keeps the one claimant it
                // already had.
                row.scoped[position] = tessera_lifecycle::WalScalar::Null;
                continue;
            }
            return Err(format!(
                "row {index} names a different value for group-scoped column '{}' than this \
                 deployment already holds for key '{key}'; a scoped value is one value per cell, \
                 so a joining row carries the stored value or omits the column",
                family.name
            ));
        }

        // ---- past this point the row is admitted, and what follows completes it ----

        // A joining row carries no descriptors and no terms: the entity's label is the one it
        // already has, put in the postings by the flush that gave it its first row.
        //
        // Here rather than in the handler: `established_collisions`'s answer is a queue drain old,
        // so a row reclassified between new and join in that gap would otherwise re-label an
        // entity or allocate one with no label at all.
        row.descriptors = Vec::new();
        row.terms = Vec::new();

        // An accepted join's omitted `render` values are backfilled here, once join-ness is
        // settled, or a joining row would leave the joined view's hot column absent while every
        // other view of the entity rendered a value.
        //
        // Before the WAL append, so replay reproduces the value rather than re-deriving it against
        // whatever the bundle holds by then.
        for (position, d) in declared.iter().enumerate() {
            if !d.render {
                continue;
            }
            let Some(supplied) = row.scalars.get(position) else {
                continue;
            };
            if !crate::session::scalar_is_absent(supplied, d) {
                continue;
            }
            // A column the entity genuinely holds nothing for is `None` here and its absence stays
            // an absence in every view.
            let Some(held) =
                held_entity_value(generation, entity, position, d, buffered, pending, &mut blob)
            else {
                continue;
            };
            row.scalars[position] = held;
        }
    }
    Ok(())
}
