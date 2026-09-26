//! An ingest batch on the handler's side: each row resolved to the item it names, compared with
//! what that item stores, and the rows that write something submitted to the write executor.
//!
//! The handler reads one generation for the whole batch, off the executor thread. It resolves
//! every row through [`tessera_lifecycle::resolve`], then reads what each named item stores, in
//! ascending entity order, and decides the row:
//!
//! - a row naming no item creates one, in the batch's view, at the row's position;
//! - a row naming an item and changing nothing stored writes nothing and is counted unchanged;
//! - a row naming an item that has no row in the batch's view, carrying a position there and
//!   changing nothing else, adds the item to the view in place where the item is newer than every
//!   row the view has flushed;
//! - any other row edits the item: its entity is deleted and a new one carries everything the item
//!   holds, the row's values over the stored ones, in every view the item is in. The item keeps its
//!   number and so its `tessera_id`.
//!
//! The rows that create, add or edit are submitted with the unique entries' sequence number the
//! handler read at, and each edit with the views its item held. The executor re-checks, from
//! memory, what can have moved since: a named item deleted, edited or added to a view, a value
//! given a holder. Where something has, nothing is written and the executor answers
//! [`ExecError::Stale`]; the handler resolves the batch once more against a newer generation, and
//! a second stale answer refuses the batch.

use std::collections::BTreeMap;

use rustc_hash::{FxHashMap, FxHashSet};
use tessera_lifecycle::resolve::{self, Identifier, Key, Refusal, RowIdentity};
use tessera_lifecycle::{
    BatchArtifacts, ExecError, IngestRow, RowOutcome, RowReceipt, Slot, UnallocatedEdit,
    UnallocatedRow, WalScalar,
};
use tessera_store::manifest::DeclaredScalar;
use tessera_store::unique::{key_of, value_text, KeyKind, UniqueKey};
use tessera_types::{EntityId, TermId, TesseraId};

use crate::engine::Engine;
use crate::write::joined::{self, BlobRow};
use crate::write::{AcceptError, SubmittedEdit};
use crate::Generation;

/// An ingest batch as the caller sent it.
#[derive(Debug, Clone)]
pub struct IngestRequest {
    pub batch_id: String,
    pub body_hash: [u8; 32],
    /// The view the batch's positions and group-scoped values are in, resolved by the caller to
    /// a declared view's id; `None` for a batch that names no view.
    pub view: Option<String>,
    pub rows: Vec<IngestRow>,
    /// The artifacts the batch's rows name in a column named for a layer.
    pub artifacts: BatchArtifacts,
}

/// What an accepted batch did.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct IngestReceipt {
    /// One per row, in request order: the item the row created or named.
    pub tessera_ids: Vec<TesseraId>,
    pub created: u64,
    /// Rows that changed an item they named.
    pub edited: u64,
    /// Rows that added an existing item to the batch's view.
    pub added: u64,
    /// Rows that named an item and changed nothing.
    pub unchanged: u64,
    /// Artifacts the batch's membership columns created, for keys no artifact held on an open
    /// layer.
    pub minted: u64,
    /// The batch id was accepted before with this body. The `tessera_id`s are the first
    /// acceptance's, and every count is zero, since this request changed nothing.
    pub replayed: bool,
    /// The rows that created an item whose label resolves to more terms than the plugin
    /// declares an item carries, by position in the request. They are stored all the same.
    pub over_bound: Vec<usize>,
}

impl IngestReceipt {
    fn of(receipt: &[RowReceipt], minted: u64, replayed: bool) -> IngestReceipt {
        let count = |outcome| {
            if replayed {
                return 0;
            }
            receipt.iter().filter(|r| r.outcome == outcome).count() as u64
        };
        IngestReceipt {
            tessera_ids: receipt
                .iter()
                .map(|r| TesseraId::new(r.tessera_id))
                .collect(),
            created: count(RowOutcome::Created),
            edited: count(RowOutcome::Edited),
            added: count(RowOutcome::Added),
            unchanged: count(RowOutcome::Unchanged),
            minted: if replayed { 0 } else { minted },
            replayed,
            over_bound: receipt
                .iter()
                .enumerate()
                .filter(|(_, r)| r.over_bound)
                .map(|(at, _)| at)
                .collect(),
        }
    }
}

/// A batch resolved against one generation: the rows it writes, and what every request row
/// became.
pub(crate) struct Planned {
    pub(crate) rows: Vec<UnallocatedRow>,
    pub(crate) edits: Vec<SubmittedEdit>,
    pub(crate) slots: Vec<Slot>,
    /// The unique values the created and edited rows set, as `(declared position, key widened)`.
    pub(crate) keys: Vec<(u16, u128)>,
    pub(crate) artifacts: BatchArtifacts,
    /// The request rows creating or editing an item whose label resolves to more terms than the
    /// plugin declares an item carries.
    pub(crate) over_bound: Vec<u32>,
}

impl Engine {
    /// Take an ingest batch: resolve it, write what it changes, and answer what it did. Nothing
    /// is written for a batch whose every row changes nothing.
    ///
    /// Blocking: a tokio handler calls this inside `spawn_blocking`.
    pub fn ingest(&self, request: IngestRequest) -> Result<IngestReceipt, AcceptError> {
        // A stepped-down node would bury what it accepts under a manifest assembled from older
        // served state; denies are not gated.
        if self.any_partition_stepped_down() {
            return Err(AcceptError::SteppedDown);
        }
        if let Some((held_hash, receipt)) = self.write.live().accepted_batch(&request.batch_id) {
            if held_hash == request.body_hash {
                return Ok(IngestReceipt::of(&receipt, 0, true));
            }
            return Err(AcceptError::Exec(ExecError::BatchConflict {
                batch_id: request.batch_id,
            }));
        }
        for attempt in 0..2 {
            let generation = self.generation();
            let unique_seq = generation.unique_live.seq();
            let submitted = self.plan_ingest(&generation, &request).and_then(|planned| {
                #[cfg(feature = "fault-injection")]
                self.switches.hold_write_check_if_wanted();
                self.write.accept_ingest(
                    planned,
                    request.batch_id.clone(),
                    request.body_hash,
                    unique_seq,
                )
            });
            match submitted {
                Ok(ingested) => {
                    return Ok(IngestReceipt::of(
                        &ingested.receipt,
                        ingested.minted,
                        ingested.replayed,
                    ))
                }
                Err(AcceptError::Exec(ExecError::Stale)) if attempt == 0 => continue,
                Err(AcceptError::Exec(ExecError::Stale)) => {
                    return Err(AcceptError::Conflict(
                        "the items this batch names changed twice while it was checked; sent \
                         again, it is decided against what they hold then"
                            .to_string(),
                    ))
                }
                Err(e) => return Err(e),
            }
        }
        unreachable!("the second attempt answers")
    }

    /// Resolve and decide every row of `request` against `generation`.
    pub(crate) fn plan_ingest(
        &self,
        generation: &Generation,
        request: &IngestRequest,
    ) -> Result<Planned, AcceptError> {
        let manifest = &generation.bundle.manifest;
        let declared = &manifest.declared_scalars;
        let meta = crate::viewport::meta_of(generation);
        let view = match request.view.as_deref() {
            None => None,
            Some(id) => Some(meta.views.iter().find(|v| v.id == id).ok_or_else(|| {
                AcceptError::UnknownView {
                    index: 0,
                    view: id.to_string(),
                }
            })?),
        };

        let mut identities = Vec::with_capacity(request.rows.len());
        for (index, row) in request.rows.iter().enumerate() {
            if row.scalars.len() > declared.len() {
                return Err(AcceptError::ScalarArity {
                    index,
                    expected: declared.len(),
                    got: row.scalars.len(),
                });
            }
            if let Some((x, y)) = row.position {
                let Some(view) = view else {
                    return Err(AcceptError::Contract(format!(
                        "row {index} carries coordinates and the batch names no view; name the \
                         view in x-tessera-view"
                    )));
                };
                if !view.quantisation.contains(x, y) {
                    return Err(AcceptError::OutsideExtent {
                        index,
                        x,
                        y,
                        quantisation: view.quantisation,
                    });
                }
            }
            identities.push(RowIdentity {
                tessera_id: row.tessera_id,
                external_id: row.external_id.clone(),
                unique: carried_keys(declared, row),
            });
        }

        let holdings = Held {
            engine: self,
            generation,
            declared,
            bound: Default::default(),
        };
        let named = match resolve::resolve(&identities, &holdings)? {
            Ok(named) => named,
            Err(refusal) => {
                return Err(AcceptError::Conflict(
                    self.refusal_text(generation, declared, request, &refusal),
                ))
            }
        };

        let scoped_families = view
            .map(|v| crate::write::scoped_families_of_view(manifest, &v.id))
            .unwrap_or_default();
        let owner_view = view.map(|v| crate::write::scoped_owner_view_of(manifest, &v.id));
        let mut rows = Vec::new();
        let mut edits: Vec<SubmittedEdit> = Vec::new();
        let mut slots = Vec::with_capacity(request.rows.len());
        let mut keys = Vec::new();
        // The request row each written row came from, and each edit, for the membership columns.
        let mut written_from: Vec<usize> = Vec::new();
        let mut edited_from: Vec<usize> = Vec::new();

        // Named items in ascending entity order, so the stored reads walk each layer forwards.
        let mut order: Vec<(EntityId, usize)> = named
            .iter()
            .enumerate()
            .filter_map(|(at, item)| item.map(|e| (e, at)))
            .collect();
        order.sort_unstable();
        let mut stored = Stored {
            bound: holdings.bound.into_inner(),
            blobs: Self::read_blobs(generation, request, &order, false)?,
            not_members: self.memberships_not_held(request, &named),
        };
        let mut decided: FxHashMap<usize, Decided> = FxHashMap::default();
        for &(entity, at) in &order {
            let decision = self.decide(
                generation,
                request,
                at,
                entity,
                view,
                owner_view.as_deref(),
                scoped_families,
                &stored,
            )?;
            decided.insert(at, decision);
        }

        // An edit carries every value the item holds, so the items edited are read whole.
        let edited: Vec<(EntityId, usize)> = order
            .iter()
            .copied()
            .filter(|(_, at)| matches!(decided.get(at), Some(Decided::Edited { .. })))
            .collect();
        if !edited.is_empty() {
            stored.blobs = Self::read_blobs(generation, request, &edited, true)?;
        }
        // Each named item's number, which its `tessera_id` is taken from and an edit keeps.
        let entities: Vec<EntityId> = order.iter().map(|(entity, _)| *entity).collect();
        let numbers: FxHashMap<EntityId, EntityId> = entities
            .iter()
            .copied()
            .zip(
                crate::edited::numbers_of(generation, &entities)
                    .map_err(|e| AcceptError::Unreadable(e.to_string()))?,
            )
            .collect();
        let shard = manifest.identity.shard_id;
        let tid_of = |entity: &EntityId| -> Result<u64, AcceptError> {
            self.identity_key
                .forward(shard, numbers[entity])
                .map(|id| id.raw())
                .map_err(|e| AcceptError::Unreadable(e.to_string()))
        };

        for (at, (row, item)) in request.rows.iter().zip(&named).enumerate() {
            match item {
                None => {
                    let (Some(view), Some((x, y))) = (view, row.position) else {
                        return Err(AcceptError::Contract(format!(
                            "row {at} names no item and carries no position, so it creates \
                             nothing; send its coordinates in the batch's view, or name the item \
                             it is about"
                        )));
                    };
                    let descriptors = self.label_of(at, row.labels.as_deref(), view)?;
                    keys.extend(identities[at].unique.iter().copied());
                    slots.push(Slot::Written {
                        row: rows.len() as u32,
                        tessera_id: None,
                    });
                    written_from.push(at);
                    rows.push(UnallocatedRow {
                        external_id: row.external_id.clone(),
                        view: view.id.clone(),
                        join: None,
                        descriptors,
                        x,
                        y,
                        scalars: row.scalars.clone(),
                        scoped: row.scoped.clone(),
                        // Resolved once the whole batch is decided, so a refused batch interns
                        // no term.
                        terms: Vec::new(),
                    });
                }
                Some(entity) => match decided.remove(&at).expect("every named row was decided") {
                    Decided::Unchanged => slots.push(Slot::Unchanged {
                        entity: *entity,
                        tessera_id: tid_of(entity)?,
                    }),
                    Decided::Added(join) => {
                        slots.push(Slot::Written {
                            row: rows.len() as u32,
                            tessera_id: Some(tid_of(entity)?),
                        });
                        written_from.push(at);
                        rows.push(*join);
                    }
                    Decided::Edited { added } => {
                        let blob = match &stored.blobs {
                            Some(blobs) => BlobRow::of(blobs.get(entity).cloned()),
                            None => BlobRow::default(),
                        };
                        let edit = self.carry(
                            generation,
                            &meta,
                            request,
                            at,
                            *entity,
                            numbers[entity],
                            view,
                            blob,
                        )?;
                        keys.extend(identities[at].unique.iter().copied());
                        slots.push(Slot::Edited {
                            edit: edits.len() as u32,
                            added,
                            tessera_id: tid_of(entity)?,
                        });
                        edited_from.push(at);
                        edits.push(edit);
                    }
                },
            }
        }

        let bound = self.declared_bounds().max_terms_per_item as usize;
        let mut over_bound = Vec::new();
        let live = self.write.live();
        for (row, at) in rows.iter_mut().zip(&written_from) {
            if row.join.is_none() {
                row.terms = live.resolve_terms(&generation.dict, &row.descriptors);
                if row.terms.len() > bound {
                    over_bound.push(*at as u32);
                }
            }
        }
        for (edit, at) in edits.iter_mut().zip(&edited_from) {
            let first = &mut edit.edit.rows[0];
            first.terms = live.resolve_terms(&generation.dict, &first.descriptors);
            if first.terms.len() > bound {
                over_bound.push(*at as u32);
            }
        }
        let artifacts = Self::written_memberships(request, &written_from, &edited_from);
        Ok(Planned {
            rows,
            edits,
            slots,
            keys,
            artifacts,
            over_bound,
        })
    }

    /// The edit a row makes of `old`: every view the item holds a row in, and the batch's view
    /// where the row places the item there; the row's label and values where it carries them and
    /// the stored ones where it leaves them out; and the item's number, which its new entity keeps.
    #[allow(clippy::too_many_arguments)]
    fn carry(
        &self,
        generation: &Generation,
        meta: &crate::viewport::EngineMeta,
        request: &IngestRequest,
        at: usize,
        old: EntityId,
        number: EntityId,
        view: Option<&crate::viewport::MetaView>,
        mut blob: BlobRow,
    ) -> Result<SubmittedEdit, AcceptError> {
        let row = &request.rows[at];
        let manifest = &generation.bundle.manifest;
        let declared = &manifest.declared_scalars;
        let unreadable = |e: &dyn std::fmt::Display| AcceptError::Unreadable(e.to_string());
        let held_views = joined::views_holding(generation, old);
        let placing = view.zip(row.position);
        let buffered = generation.buffer.get(old);
        let own_view = match (placing, buffered) {
            (Some((view, _)), _) => view.id.clone(),
            (None, Some(item)) => item.view.clone(),
            (None, None) => match held_views.first() {
                Some(view) => view.clone(),
                None => {
                    let tid = self
                        .tessera_id_in(generation, old)
                        .map_err(|e| unreadable(&e))?
                        .raw();
                    return Err(AcceptError::Contract(format!(
                        "row {at} changes item {tid}, which has a row in no view, and carries no \
                         position; send its coordinates in a view"
                    )));
                }
            },
        };
        let mut views: Vec<String> = vec![own_view.clone()];
        views.extend(held_views.iter().filter(|v| **v != own_view).cloned());

        let descriptors = match &row.labels {
            Some(labels) => self
                .plugin
                .terms_of_labels(labels)
                .map_err(|e| AcceptError::Contract(format!("row {at}, access: {e}")))?,
            None => self.stored_label(generation, old)?,
        };
        let carried = |position: usize| {
            position < row.scalars.len() && !row.omitted.contains(&position)
        };
        let scalars: Vec<WalScalar> = declared
            .iter()
            .enumerate()
            .map(|(position, d)| match carried(position) {
                true => row.scalars[position].clone(),
                false => joined::held_entity_value(
                    generation, old, position, d, buffered, &mut blob,
                )
                .unwrap_or(WalScalar::Null),
            })
            .collect();
        let external_id = match &row.external_id {
            Some(id) => Some(id.clone()),
            None => self.stored_external_id(generation, old)?,
        };
        // The batch's view's owner addresses the row's group-scoped values; every other view's
        // cells are carried.
        let batch_owner = view.map(|v| crate::write::scoped_owner_view_of(manifest, &v.id));
        let declared_len = declared.len();

        let mut rows = Vec::with_capacity(views.len());
        // A key shared by several views is one cell per family, carried by the first row that
        // addresses it.
        let mut owners_carried: Vec<String> = Vec::new();
        for view_id in &views {
            let (x, y) = match placing {
                Some((view, position)) if view.id == *view_id => position,
                _ => {
                    let frame = meta
                        .views
                        .iter()
                        .find(|v| v.id == *view_id)
                        .ok_or_else(|| AcceptError::UnknownView {
                            index: at,
                            view: view_id.clone(),
                        })?;
                    self.stored_xy(generation, old, frame)?.ok_or_else(|| {
                        AcceptError::Unreadable(format!(
                            "row {at}'s item has no readable row in view '{view_id}'"
                        ))
                    })?
                }
            };
            let families = crate::write::scoped_families_of_view(manifest, view_id);
            let owner = crate::write::scoped_owner_view_of(manifest, view_id);
            let from_row = batch_owner.as_deref() == Some(owner.as_str());
            let repeated = owners_carried.contains(&owner);
            owners_carried.push(owner.clone());
            let scoped: Vec<WalScalar> = families
                .iter()
                .enumerate()
                .map(|(position, family)| {
                    if repeated {
                        return WalScalar::Null;
                    }
                    let supplied = row
                        .scoped
                        .get(position)
                        .filter(|_| from_row && !row.omitted.contains(&(declared_len + position)));
                    match supplied {
                        Some(value) => value.clone(),
                        None => joined::held_scoped_value(
                            generation,
                            old,
                            position,
                            family,
                            &joined::declared_of_scoped(family),
                            &owner,
                        )
                        .unwrap_or(WalScalar::Null),
                    }
                })
                .collect();
            let first = rows.is_empty();
            rows.push(UnallocatedRow {
                external_id: if first { external_id.clone() } else { None },
                view: view_id.clone(),
                join: None,
                descriptors: if first { descriptors.clone() } else { Vec::new() },
                x,
                y,
                scalars: scalars.clone(),
                scoped,
                terms: Vec::new(),
            });
        }
        Ok(SubmittedEdit {
            edit: UnallocatedEdit { old, number, rows },
            held_views,
        })
    }

    /// An item's stored label as descriptors: its buffered row's terms, or the terms a flush wrote
    /// for it, each read back to the descriptor it was resolved from.
    fn stored_label(
        &self,
        generation: &Generation,
        entity: EntityId,
    ) -> Result<Vec<Vec<u8>>, AcceptError> {
        let terms: Vec<TermId> = match generation.buffer.get(entity) {
            Some(item) => item.terms.clone(),
            None => joined::flushed_terms_of(generation, entity).ok_or_else(|| {
                AcceptError::Unreadable("an edited item's label could not be read".to_string())
            })?,
        };
        let novel: FxHashSet<TermId> = terms
            .iter()
            .copied()
            .filter(|term| term.raw() >= generation.dict.len())
            .collect();
        let novel = if novel.is_empty() {
            FxHashMap::default()
        } else {
            self.write.live().descriptors_of(&novel)
        };
        terms
            .iter()
            .map(|term| {
                generation
                    .dict
                    .descriptor(*term)
                    .map(<[u8]>::to_vec)
                    .or_else(|| novel.get(term).cloned())
                    .ok_or_else(|| {
                        AcceptError::Unreadable(
                            "an edited item's label names a term with no descriptor".to_string(),
                        )
                    })
            })
            .collect()
    }

    /// The item's position in `view`'s frame, or `None` where it has no row there: a buffered
    /// row's coordinates as sent, or the centre of a flushed row's cell, which quantises back to
    /// the cell.
    fn stored_xy(
        &self,
        generation: &Generation,
        entity: EntityId,
        view: &crate::viewport::MetaView,
    ) -> Result<Option<(f64, f64)>, AcceptError> {
        if let Some(item) = generation
            .buffer
            .rows_of(entity)
            .find(|item| item.view == view.id)
        {
            return Ok(Some((item.x, item.y)));
        }
        let q = view.quantisation;
        Ok(self.stored_position(generation, entity, view)?.map(|(fx, fy)| {
            (
                tessera_spatial::unfixed32(fx, q.x_min, q.x_max),
                tessera_spatial::unfixed32(fy, q.y_min, q.y_max),
            )
        }))
    }

    /// A new item's label as descriptors: the row's, or the view's default where it leaves its
    /// label out.
    fn label_of(
        &self,
        at: usize,
        labels: Option<&[Vec<u8>]>,
        view: &crate::viewport::MetaView,
    ) -> Result<Vec<Vec<u8>>, AcceptError> {
        let labels: Vec<Vec<u8>> = match labels {
            Some(labels) => labels.to_vec(),
            None => match &view.point_default {
                Some(default) => vec![default.as_bytes().to_vec()],
                None => {
                    return Err(AcceptError::Contract(format!(
                        "row {at} creates an item with no access label, and view '{}' declares \
                         no `point_visibility.default`; label the row, or declare the default",
                        view.id
                    )))
                }
            },
        };
        self.plugin
            .terms_of_labels(&labels)
            .map_err(|e| AcceptError::Contract(format!("row {at}, access: {e}")))
    }

    /// Decide one row naming `entity` against what the item stores.
    #[allow(clippy::too_many_arguments)]
    fn decide(
        &self,
        generation: &Generation,
        request: &IngestRequest,
        at: usize,
        entity: EntityId,
        view: Option<&crate::viewport::MetaView>,
        owner_view: Option<&str>,
        scoped_families: &[tessera_store::manifest::ScopedScalar],
        stored: &Stored,
    ) -> Result<Decided, AcceptError> {
        let row = &request.rows[at];
        let manifest = &generation.bundle.manifest;
        let declared = &manifest.declared_scalars;
        let buffered = generation.buffer.get(entity);
        let mut blob = match &stored.blobs {
            Some(blobs) => BlobRow::of(blobs.get(&entity).cloned()),
            None => BlobRow::default(),
        };
        let membership = || match stored.not_members.contains_key(&at) {
            true => Decided::Edited { added: false },
            false => Decided::Unchanged,
        };
        let carried = |position: usize| !row.omitted.contains(&position);

        if let Some(labels) = &row.labels {
            let descriptors = self
                .plugin
                .terms_of_labels(labels)
                .map_err(|e| AcceptError::Contract(format!("row {at}, access: {e}")))?;
            // A novel descriptor is on no stored label.
            let supplied = self.write.live().lookup_terms(&generation.dict, &descriptors);
            let held: Option<Vec<TermId>> = match buffered {
                Some(item) => Some(item.terms.clone()),
                None => joined::flushed_terms_of(generation, entity),
            };
            let same = match (supplied, held) {
                (Some(mut supplied), Some(mut held)) => {
                    supplied.sort_unstable();
                    supplied.dedup();
                    held.sort_unstable();
                    held.dedup();
                    held == supplied
                }
                _ => false,
            };
            if !same {
                return Ok(Decided::Edited { added: false });
            }
        }

        // The row's external id named an item or nothing; an item it named is this one, or the
        // batch would have been refused as naming two.
        if let Some(external_id) = &row.external_id {
            if !stored.bound.contains(external_id) {
                return Ok(Decided::Edited { added: false });
            }
        }

        // Each carried value against the stored one; a null is compared as no value.
        for (position, d) in declared.iter().enumerate() {
            let Some(value) = row.scalars.get(position).filter(|_| carried(position)) else {
                continue;
            };
            let value = (!joined::scalar_is_absent(value, d)).then_some(value);
            let held = joined::held_entity_value(generation, entity, position, d, buffered, &mut blob);
            let same = match (value, &held) {
                (Some(value), Some(held)) => {
                    joined::supplied_as_stored(generation, value, d).same_as(held)
                }
                (None, None) => true,
                _ => false,
            };
            if !same {
                return Ok(Decided::Edited { added: false });
            }
        }

        let (Some(view), Some((x, y))) = (view, row.position) else {
            // No position: nothing about the item's views changes.
            if let Some(owner_view) = owner_view {
                if scoped_against_held(generation, entity, row, scoped_families, owner_view, false)
                    .is_none()
                {
                    return Ok(Decided::Edited { added: false });
                }
            }
            return Ok(membership());
        };
        let owner_view = owner_view.expect("a view has an owner view");
        let adding = match self.stored_position(generation, entity, view)? {
            Some(held) => {
                if held != fixed(view.quantisation, x, y) {
                    return Ok(Decided::Edited { added: false });
                }
                false
            }
            None => true,
        };
        let Some(repeated) =
            scoped_against_held(generation, entity, row, scoped_families, owner_view, adding)
        else {
            return Ok(Decided::Edited { added: false });
        };
        if !adding {
            return Ok(membership());
        }
        // A flush places rows only above a view's newest, so an item older than that moves to a
        // new entity to join the view; so does one whose row changes a membership.
        if !joined::joins_in_place(generation, entity, &view.id)
            || stored.not_members.contains_key(&at)
        {
            return Ok(Decided::Edited { added: true });
        }
        // The row a view gains carries the item's values. A rendered value is read from the item
        // where the row leaves it out, so the view renders what the item's other views do.
        let scalars: Vec<WalScalar> = declared
            .iter()
            .enumerate()
            .map(|(position, d)| match row.scalars.get(position) {
                Some(value) if carried(position) => value.clone(),
                _ if d.render => {
                    joined::held_entity_value(generation, entity, position, d, buffered, &mut blob)
                        .unwrap_or(WalScalar::Null)
                }
                _ => WalScalar::Null,
            })
            .collect();
        // A cell this view's key already holds keeps the one value; the row's copy is dropped.
        let mut scoped = row.scoped.clone();
        for position in repeated {
            scoped[position] = WalScalar::Null;
        }
        let external_id = match &row.external_id {
            Some(id) => Some(id.clone()),
            None => self.stored_external_id(generation, entity)?,
        };
        Ok(Decided::Added(Box::new(UnallocatedRow {
            external_id,
            view: view.id.clone(),
            join: Some(entity),
            descriptors: Vec::new(),
            x,
            y,
            scalars,
            scoped,
            terms: Vec::new(),
        })))
    }

    /// The rows naming an item that an artifact their membership columns name does not hold,
    /// with that artifact's layer: such a row would change what the item belongs to. One read of
    /// the artifact store for the batch.
    fn memberships_not_held(
        &self,
        request: &IngestRequest,
        named: &[Option<EntityId>],
    ) -> FxHashMap<usize, String> {
        if request.artifacts.memberships.is_empty() {
            return FxHashMap::default();
        }
        self.write.live().with_artifacts(|store| {
            let mut out = FxHashMap::default();
            for membership in &request.artifacts.memberships {
                let record = store
                    .ordinal_of_key(
                        &membership.layer,
                        membership.level,
                        membership.view.as_deref(),
                        &membership.key,
                    )
                    .and_then(|ordinal| store.get(&membership.layer, membership.level, ordinal));
                for &at in &membership.rows {
                    let at = at as usize;
                    let Some(entity) = named.get(at).copied().flatten() else {
                        continue;
                    };
                    let held = u32::try_from(entity.raw())
                        .ok()
                        .zip(record)
                        .is_some_and(|(member, record)| store.members_of(record).contains(member));
                    if !held {
                        out.entry(at).or_insert_with(|| membership.layer.clone());
                    }
                }
            }
            out
        })
    }

    /// The record-blob rows of the named items, read in one walk over the blocks they sit in:
    /// every item's where `whole`, for the edits that carry them, and otherwise only where a row
    /// carries a column stored there, for the comparison. `None` where a comparison's walk could
    /// not be made, and each row is then read alone; an edit's is refused, since it would carry
    /// nothing where a value is stored.
    fn read_blobs(
        generation: &Generation,
        request: &IngestRequest,
        order: &[(EntityId, usize)],
        whole: bool,
    ) -> Result<Option<FxHashMap<EntityId, Vec<tessera_filter::RecordField>>>, AcceptError> {
        let manifest = &generation.bundle.manifest;
        let resident: Vec<usize> = manifest
            .declared_scalars
            .iter()
            .enumerate()
            .filter(|(_, d)| crate::filter::blob_resident(d, &manifest.vocabularies))
            .map(|(at, _)| at)
            .collect();
        let wanted: croaring::Bitmap = order
            .iter()
            .filter(|(_, at)| {
                let row = &request.rows[*at];
                whole
                    || resident
                        .iter()
                        .any(|p| *p < row.scalars.len() && !row.omitted.contains(p))
            })
            .filter_map(|(entity, _)| u32::try_from(entity.raw()).ok())
            .collect();
        let mut blobs = FxHashMap::default();
        if wanted.is_empty() || resident.is_empty() {
            return Ok(Some(blobs));
        }
        let read = generation
            .filter_columns
            .records()
            .for_each_row_in(&wanted, &mut |entity, fields| {
                blobs.insert(EntityId::new(u64::from(entity)), fields);
                Ok(())
            });
        match read {
            Ok(()) => Ok(Some(blobs)),
            Err(e) if whole => Err(AcceptError::Unreadable(format!("the record blob: {e}"))),
            Err(_) => Ok(None),
        }
    }

    /// The item's external id: its own buffered row's, or the one a flush bound.
    fn stored_external_id(
        &self,
        generation: &Generation,
        entity: EntityId,
    ) -> Result<Option<Vec<u8>>, AcceptError> {
        if let Some(item) = generation.buffer.get(entity) {
            return Ok(item.external_id.clone());
        }
        self.external_id_of_in(generation, entity)
            .map_err(|e| AcceptError::Unreadable(e.to_string()))
    }

    /// The item's position in `view`, quantised, or `None` where it has no row there.
    fn stored_position(
        &self,
        generation: &Generation,
        entity: EntityId,
        view: &crate::viewport::MetaView,
    ) -> Result<Option<(u32, u32)>, AcceptError> {
        if let Some(item) = generation
            .buffer
            .rows_of(entity)
            .find(|item| item.view == view.id)
        {
            return Ok(Some(fixed(view.quantisation, item.x, item.y)));
        }
        for partition in generation.bundle.partitions.values() {
            let Some(view_data) = partition.views.get(&view.id) else {
                continue;
            };
            let found = crate::viewport::segment_row_of(&view.id, view_data, entity)
                .map_err(|e| AcceptError::Unreadable(e.to_string()))?;
            if let Some((segment, local)) = found {
                return Ok(Some(tessera_spatial::unsplit32(
                    tessera_types::MortonCode::new(segment.morton.u32()[local]),
                    segment.columns.residual()[local],
                )));
            }
        }
        Ok(None)
    }

    /// The batch's membership columns, over the rows it writes: a row writing nothing has been
    /// found to be a member of every artifact it names already. `written_from` is the request row
    /// each written row came from and `edited_from` each edit's, numbered on from the rows.
    fn written_memberships(
        request: &IngestRequest,
        written_from: &[usize],
        edited_from: &[usize],
    ) -> BatchArtifacts {
        let position: BTreeMap<u32, u32> = written_from
            .iter()
            .chain(edited_from)
            .enumerate()
            .map(|(written, at)| (*at as u32, written as u32))
            .collect();
        let memberships = request
            .artifacts
            .memberships
            .iter()
            .filter_map(|membership| {
                let rows: Vec<u32> = membership
                    .rows
                    .iter()
                    .filter_map(|at| position.get(at).copied())
                    .collect();
                (!rows.is_empty()).then(|| tessera_lifecycle::BatchMembership {
                    rows,
                    ..membership.clone()
                })
            })
            .collect();
        BatchArtifacts {
            memberships,
            edges: request.artifacts.edges.clone(),
        }
    }

    /// A refusal as the caller reads it: rows by position, values as sent, items by
    /// `tessera_id`.
    fn refusal_text(
        &self,
        generation: &Generation,
        declared: &[DeclaredScalar],
        request: &IngestRequest,
        refusal: &Refusal,
    ) -> String {
        let tid = |entity: EntityId| {
            crate::unique::tessera_id_text(self, generation, entity)
        };
        let value = |row: usize, field: Identifier| match field {
            Identifier::TesseraId => "its tessera_id".to_string(),
            Identifier::ExternalId => "its external_id".to_string(),
            Identifier::Unique(at) => {
                let at = usize::from(at);
                let text = request.rows[row]
                    .scalars
                    .get(at)
                    .map(value_text)
                    .unwrap_or_default();
                format!("'{}' = {text}", declared[at].name)
            }
        };
        match refusal {
            Refusal::NamesTwo { row, named } => {
                let mut seen = Vec::new();
                let parts: Vec<String> = named
                    .iter()
                    .filter(|(_, entity)| {
                        let fresh = !seen.contains(entity);
                        seen.push(*entity);
                        fresh
                    })
                    .map(|(field, entity)| {
                        format!("{} names item {}", value(*row, *field), tid(*entity))
                    })
                    .collect();
                format!(
                    "row {row} names more than one item: {}; send values that name one item",
                    parts.join(" and ")
                )
            }
            Refusal::UnknownTesseraId { row } => {
                let id = request.rows[*row]
                    .tessera_id
                    .map(|t| t.raw())
                    .unwrap_or_default();
                format!(
                    "row {row}'s tessera_id {id} names no item; leave tessera_id out to create \
                     an item, which is given its tessera_id when it is created"
                )
            }
            Refusal::OneItemTwice { rows, item } => format!(
                "rows {} and {} both name item {}; send one row per item",
                rows[0],
                rows[1],
                tid(*item)
            ),
            Refusal::OneValueTwice { rows, field } => format!(
                "rows {} and {} both carry {}; a unique value is held by one item, so send it \
                 in one row",
                rows[0],
                rows[1],
                value(rows[1], *field).trim_start_matches("its ")
            ),
        }
    }
}

/// What a batch's named items store, read once for the whole batch rather than per row.
struct Stored {
    /// The external ids the batch's rows carry that name an item.
    bound: FxHashSet<Vec<u8>>,
    /// Each named item's record-blob row, where a row carries a column stored there.
    blobs: Option<FxHashMap<EntityId, Vec<tessera_filter::RecordField>>>,
    /// The rows whose membership columns name an artifact that does not hold their item.
    not_members: FxHashMap<usize, String>,
}

/// What one row naming an item does to it.
enum Decided {
    Unchanged,
    /// The row adds the item to the batch's view in place, as this row.
    Added(Box<UnallocatedRow>),
    /// The row changes the item, which moves to a new entity. `added` where adding it to the
    /// batch's view is the one change.
    Edited { added: bool },
}

/// The unique values a row carries, as the resolver takes them. A null never identifies an item.
fn carried_keys(declared: &[DeclaredScalar], row: &IngestRow) -> Vec<(u16, Key)> {
    declared
        .iter()
        .enumerate()
        .filter(|(at, d)| d.unique && !row.omitted.contains(at))
        .filter_map(|(at, d)| {
            let value = row.scalars.get(at)?;
            key_of(d.arrow_type, value).map(|key| (at as u16, key.widen()))
        })
        .collect()
}

/// A position in a view's frame, as its segment stores it.
fn fixed(q: tessera_store::manifest::Quantisation, x: f64, y: f64) -> (u32, u32) {
    (
        tessera_spatial::fixed32(x, q.x_min, q.x_max),
        tessera_spatial::fixed32(y, q.y_min, q.y_max),
    )
}

/// The group-scoped values a row carries against the cells they address: the positions whose held
/// value the row repeats, or `None` where one differs. An empty cell takes a value where the row
/// adds the item to the view (`adding`), and differs from one otherwise. A cell holding prose that
/// a flush has made uncomparable differs from any value.
fn scoped_against_held(
    generation: &Generation,
    entity: EntityId,
    row: &IngestRow,
    families: &[tessera_store::manifest::ScopedScalar],
    owner_view: &str,
    adding: bool,
) -> Option<Vec<usize>> {
    let declared_len = generation.bundle.manifest.declared_scalars.len();
    let mut repeated = Vec::new();
    for (position, family) in families.iter().enumerate() {
        if row.omitted.contains(&(declared_len + position)) {
            continue;
        }
        let Some(supplied) = row.scoped.get(position) else {
            continue;
        };
        let d = joined::declared_of_scoped(family);
        let supplied = (!joined::scalar_is_absent(supplied, &d)).then_some(supplied);
        let held = joined::held_scoped_value(generation, entity, position, family, &d, owner_view);
        let prose = held.is_none()
            && family.arrow_type == tessera_spatial::tiler::ScalarType::Text
            && joined::flushed_scoped_text_present(generation, entity, family, owner_view);
        match (supplied, held) {
            (None, None) if !prose => {}
            (Some(_), None) if adding && !prose => {}
            (Some(value), Some(held))
                if joined::supplied_as_stored(generation, value, &d).same_as(&held) =>
            {
                repeated.push(position)
            }
            _ => return None,
        }
    }
    Some(repeated)
}

/// Who holds what in one generation: the unique indexes and their live entries, the external ids
/// bound, and the `tessera_id`s issued, deleted items left out of each.
struct Held<'a> {
    engine: &'a Engine,
    generation: &'a Generation,
    declared: &'a [DeclaredScalar],
    /// The external ids found to name an item, as the resolver asked about them.
    bound: std::cell::RefCell<FxHashSet<Vec<u8>>>,
}

impl resolve::Holdings for Held<'_> {
    type Error = AcceptError;

    fn holders(&self, field: u16, keys: &[Key]) -> Result<Vec<Vec<EntityId>>, AcceptError> {
        let d = &self.declared[usize::from(field)];
        let kind = KeyKind::of(d.arrow_type).expect("a unique column's type takes a key");
        let keys: Vec<UniqueKey> = keys
            .iter()
            .map(|k| UniqueKey::of_widened(kind, *k))
            .collect();
        let found = crate::unique::holders(self.generation, &d.name, &keys)
            .map_err(|e| AcceptError::Unreadable(e.to_string()))?;
        let mut out = vec![Vec::new(); keys.len()];
        for (at, entity) in found {
            out[at].push(entity);
        }
        Ok(out)
    }

    /// An external id bound since this generation was taken names an item it does not hold yet,
    /// which is answered as stale so the batch is resolved again against a newer one.
    fn external_holders(&self, ids: &[&[u8]]) -> Result<Vec<Option<EntityId>>, AcceptError> {
        let owned: Vec<Vec<u8>> = ids.iter().map(|id| id.to_vec()).collect();
        let found = self
            .engine
            .external_ids_in(self.generation, &owned)
            .map_err(|e| AcceptError::Unreadable(e.to_string()))?;
        let mut out = Vec::with_capacity(found.len());
        let mut bound = self.bound.borrow_mut();
        for (id, entity) in owned.into_iter().zip(found) {
            match entity {
                Some(e) if self.generation.overlay.is_deleted(e) => out.push(None),
                Some(e) if !crate::control::holds_item(self.generation, e) => {
                    return Err(AcceptError::Exec(ExecError::Stale))
                }
                Some(e) => {
                    bound.insert(id);
                    out.push(Some(e));
                }
                None => out.push(None),
            }
        }
        Ok(out)
    }

    fn tessera_holders(&self, ids: &[TesseraId]) -> Result<Vec<Option<EntityId>>, AcceptError> {
        let high_water = self.engine.allocator_high_water();
        Ok(self
            .engine
            .tessera_ids_in(self.generation, ids)
            .map_err(|e| AcceptError::Unreadable(e.to_string()))?
            .into_iter()
            .map(|entity| entity.filter(|e| e.raw() < high_water))
            .collect())
    }
}
