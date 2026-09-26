use super::*;

/// What one values batch's fill rule produced: the cells to hold until the flush writes them, and
/// the counts the acknowledgement carries.
#[derive(Default)]
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

/// Where each column a values batch names lands: a declared scalar first, then one of the batch
/// view's group-scoped families. Refuses a name in neither, a `render` column, and any column on
/// a batch that names no view. Asked again here after the door asked it, since the door read a
/// generation this pass may have moved past.
pub(super) fn values_columns(
    manifest: &tessera_store::manifest::Manifest,
    request: &tessera_lifecycle::ValuesRequest,
) -> Result<Vec<ValuesColumn>, ExecError> {
    let declared = &manifest.declared_scalars;
    // A cell is written by its view's flush pass; a batch naming no view fills none.
    let Some(view) = request.view.as_deref() else {
        if let Some(name) = request.columns.first() {
            return Err(ExecError::ValuesRefused {
                detail: format!(
                    "column '{name}' fills a cell and this batch names no view to write it by; \
                     name one in x-tessera-view"
                ),
            });
        }
        return Ok(Vec::new());
    };
    let families = scoped_families_of_view(manifest, view);
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
                 set holds view '{view}'; declare the column, or name the view whose key \
                 addresses the cell"
            ),
        });
    }
    Ok(columns)
}

/// Apply the fill rule to one values batch, whose `columns` are [`values_columns`]' answer: the
/// cells nothing holds, refusing on the first cell held differently. Claims each cell from three
/// sources in order: the buffered row, an earlier batch's unflushed fill, and the flushed homes.
/// An absent cell has no claimant in any of them, so extents stay disjoint per column when the
/// flush writes them.
///
/// A row index, a column name and a key reach the caller; nothing else does (I10).
pub(super) fn plan_fills(
    generation: &Generation,
    request: &tessera_lifecycle::ValuesRequest,
    columns: &[ValuesColumn],
) -> Result<PlannedFills, ExecError> {
    let manifest = &generation.bundle.manifest;
    let declared = &manifest.declared_scalars;
    let Some(view) = request.view.as_deref() else {
        return Ok(PlannedFills::default());
    };
    let families = scoped_families_of_view(manifest, view);
    let owner_view = scoped_owner_view_of(manifest, view);
    let key = key_of_owner_view(&owner_view);

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
        let mut blob = crate::write::joined::BlobRow::default();
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
                    if crate::write::joined::scalar_is_absent(supplied, d) {
                        continue;
                    }
                    let stored = crate::write::joined::held_entity_value(
                        generation, entity, *at, d, buffered, pending, &mut blob,
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
                    let d = crate::write::joined::declared_of_scoped(family);
                    if crate::write::joined::scalar_is_absent(supplied, &d) {
                        continue;
                    }
                    let stored = crate::write::joined::held_scoped_value(
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
                        && crate::write::joined::flushed_scoped_text_present(
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
                    view: view.to_string(),
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
                    view: view.to_string(),
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
/// What this door adds to the shared grouping: the entities are the ones the caller's rows named,
/// and a membership naming a row the batch does not carry is a refusal rather than an assignment
/// this door could look up. It is one batch, so there is no entry to blame and the index the
/// grouping carries is dropped.
pub(super) fn values_growth_records(
    memberships: &[tessera_lifecycle::ResolvedMembership],
    rows: &[tessera_lifecycle::IncomingValues],
    store: &tessera_lifecycle::ArtifactStore,
) -> Result<Vec<WalRecord>, String> {
    // Every values row names an entity that exists, so any of them could be restating a membership
    // the artifact holds. Read before the batch is applied: [`Executor::commit_values`] holds the
    // executor's one lock across the preparation.
    let held = HeldMembers {
        store,
        restating: None,
    };
    Ok(
        grouped_growth(std::iter::once(memberships), &row_entity_of(rows), Some(held))?
            .into_iter()
            .map(|(record, _)| record)
            .collect(),
    )
}

/// The artifacts one values batch's layer columns named and no artifact holds: [`mint_plan`]'s
/// twin for the values door, on [`values_growth_records`]'s reading of the rows.
///
/// An unknown key creates the artifact on an `open` layer, carrying the batch's own rows as its
/// first members.
pub(super) fn values_mint_plan(
    memberships: &[tessera_lifecycle::ResolvedMembership],
    rows: &[tessera_lifecycle::IncomingValues],
) -> Result<MintPlan, String> {
    grouped_mints(std::iter::once(memberships), &row_entity_of(rows))
}

/// This door's entity lookup: a batch is one source, and a row index is a position in it.
fn row_entity_of(
    rows: &[tessera_lifecycle::IncomingValues],
) -> impl Fn(usize, u32) -> Option<EntityId> + '_ {
    |_, row| rows.get(row as usize).map(|row| row.entity)
}
