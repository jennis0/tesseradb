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
//!   changing nothing else, adds the item to the view;
//! - any other row would change the item, and editing an item is not built yet, so the batch is
//!   refused.
//!
//! The rows that create or add are submitted with the unique entries' sequence number the handler
//! read at. The executor re-checks, from memory, what can have moved since: a named item deleted
//! or added to the view, a value given a holder. Where something has, nothing is written and the
//! executor answers [`ExecError::Stale`]; the handler resolves the batch once more against a
//! newer generation, and a second stale answer refuses the batch.

use std::collections::BTreeMap;

use rustc_hash::FxHashMap;
use tessera_lifecycle::resolve::{self, Identifier, Key, Refusal, RowIdentity};
use tessera_lifecycle::{
    BatchArtifacts, ExecError, IngestRow, RowOutcome, RowReceipt, Slot, UnallocatedRow, WalScalar,
};
use tessera_store::manifest::DeclaredScalar;
use tessera_store::unique::{key_of, value_text, KeyKind, UniqueKey};
use tessera_types::{EntityId, TermId, TesseraId};

use crate::engine::Engine;
use crate::write::joined::{self, BlobRow};
use crate::write::AcceptError;
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
    /// Rows that changed an item. Zero: editing an item is not built yet.
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
    fn of(receipt: &[RowReceipt], minted: u64, over_bound: Vec<usize>) -> IngestReceipt {
        let count = |outcome| receipt.iter().filter(|r| r.outcome == outcome).count() as u64;
        IngestReceipt {
            tessera_ids: receipt
                .iter()
                .map(|r| TesseraId::new(r.tessera_id))
                .collect(),
            created: count(RowOutcome::Created),
            edited: 0,
            added: count(RowOutcome::Added),
            unchanged: count(RowOutcome::Unchanged),
            minted,
            replayed: false,
            over_bound,
        }
    }

    fn replay(receipt: &[RowReceipt]) -> IngestReceipt {
        IngestReceipt {
            tessera_ids: receipt
                .iter()
                .map(|r| TesseraId::new(r.tessera_id))
                .collect(),
            created: 0,
            edited: 0,
            added: 0,
            unchanged: 0,
            minted: 0,
            replayed: true,
            over_bound: Vec::new(),
        }
    }
}

/// A batch resolved against one generation: the rows it writes, and what every request row
/// became.
pub(crate) struct Planned {
    pub(crate) rows: Vec<UnallocatedRow>,
    pub(crate) slots: Vec<Slot>,
    /// The unique values the created rows set, as `(declared position, key widened)`.
    pub(crate) keys: Vec<(u16, u128)>,
    pub(crate) artifacts: BatchArtifacts,
    over_bound: Vec<usize>,
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
                return Ok(IngestReceipt::replay(&receipt));
            }
            return Err(AcceptError::Exec(ExecError::BatchConflict {
                batch_id: request.batch_id,
            }));
        }
        for attempt in 0..2 {
            let generation = self.generation();
            let unique_seq = generation.unique_live.seq();
            let planned = self.plan_ingest(&generation, &request)?;
            if planned.rows.is_empty() && planned.artifacts.is_empty() {
                let receipt: Vec<RowReceipt> = planned
                    .slots
                    .iter()
                    .map(|slot| match slot {
                        Slot::Unchanged(entity) => RowReceipt {
                            outcome: RowOutcome::Unchanged,
                            tessera_id: self.issued_tessera_id(&generation, *entity),
                        },
                        Slot::Written(_) => unreachable!("a batch writing nothing has no row"),
                    })
                    .collect();
                return Ok(IngestReceipt::of(&receipt, 0, planned.over_bound));
            }
            #[cfg(feature = "fault-injection")]
            self.switches.hold_write_check_if_wanted();
            let over_bound = planned.over_bound.clone();
            match self.write.accept_ingest(
                planned,
                request.batch_id.clone(),
                request.body_hash,
                unique_seq,
            ) {
                Ok(ingested) if ingested.replayed => {
                    return Ok(IngestReceipt::replay(&ingested.receipt))
                }
                Ok(ingested) => {
                    return Ok(IngestReceipt::of(
                        &ingested.receipt,
                        ingested.minted,
                        over_bound,
                    ))
                }
                Err(AcceptError::Exec(ExecError::Stale)) if attempt == 0 => continue,
                Err(AcceptError::Exec(ExecError::Stale)) => {
                    return Err(AcceptError::Conflict(
                        "the items this batch names changed while it was checked; send it again"
                            .to_string(),
                    ))
                }
                Err(e) => return Err(e),
            }
        }
        unreachable!("the second attempt answers")
    }

    /// Submit rows carrying a label and a position, and answer the entity each row created or
    /// named. For callers holding rows already shaped for the executor; every row names its own
    /// view, and all of them the same one. A null value is left out: on a row naming an item it
    /// keeps what the item stores.
    pub fn accept_ingest(
        &self,
        rows: Vec<UnallocatedRow>,
        batch_id: String,
        body_hash: [u8; 32],
    ) -> Result<Vec<EntityId>, AcceptError> {
        self.accept_ingest_joining(rows, batch_id, body_hash, BatchArtifacts::default())
            .map(|(entities, _)| entities)
    }

    /// [`Self::accept_ingest`], with the artifacts the rows name in a layer's column, answering
    /// how many artifacts the batch minted beside the entities.
    pub fn accept_ingest_joining(
        &self,
        rows: Vec<UnallocatedRow>,
        batch_id: String,
        body_hash: [u8; 32],
        artifacts: BatchArtifacts,
    ) -> Result<(Vec<EntityId>, u64), AcceptError> {
        let view = rows.first().map(|row| row.view.clone());
        if let Some((index, row)) = rows
            .iter()
            .enumerate()
            .find(|(_, row)| Some(&row.view) != view.as_ref())
        {
            return Err(AcceptError::UnknownView {
                index,
                view: row.view.clone(),
            });
        }
        let declared = self.generation().bundle.manifest.declared_scalars.len();
        let rows = rows
            .into_iter()
            .map(|row| {
                let null = |value: &WalScalar| matches!(value, WalScalar::Null);
                let omitted = (0..declared)
                    .filter(|at| row.scalars.get(*at).is_none_or(null))
                    .chain(
                        (0..row.scoped.len())
                            .filter(|at| null(&row.scoped[*at]))
                            .map(|at| declared + at),
                    )
                    .collect();
                IngestRow {
                    tessera_id: None,
                    external_id: row.external_id,
                    labels: Some(row.descriptors),
                    position: Some((row.x, row.y)),
                    scalars: row.scalars,
                    scoped: row.scoped,
                    omitted,
                }
            })
            .collect();
        let receipt = self.ingest(IngestRequest {
            batch_id,
            body_hash,
            view,
            rows,
            artifacts,
        })?;
        let shard = self.generation().bundle.manifest.identity.shard_id;
        let entities = receipt
            .tessera_ids
            .iter()
            .map(|id| {
                let (id_shard, entity) = self.identity_key.invert(*id);
                debug_assert_eq!(id_shard, shard, "a receipt names this bundle's items");
                entity
            })
            .collect();
        Ok((entities, receipt.minted))
    }

    /// An item's `tessera_id`. Every entity the allocator issues is inside the identity space.
    fn issued_tessera_id(&self, generation: &Generation, entity: EntityId) -> u64 {
        self.identity_key
            .forward(generation.bundle.manifest.identity.shard_id, entity)
            .expect("an issued entity is inside the identity space")
            .raw()
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
        let bound = self.declared_bounds().max_terms_per_item;

        let mut rows = Vec::new();
        let mut slots = Vec::with_capacity(request.rows.len());
        let mut keys = Vec::new();
        let mut over_bound = Vec::new();
        // The request row each written row came from, for the membership columns.
        let mut written_from: Vec<usize> = Vec::new();

        // Named items in ascending entity order, so the stored reads walk each layer forwards.
        let mut order: Vec<(EntityId, usize)> = named
            .iter()
            .enumerate()
            .filter_map(|(at, item)| item.map(|e| (e, at)))
            .collect();
        order.sort_unstable();
        let mut decided: FxHashMap<usize, Decided> = FxHashMap::default();
        for (entity, at) in order {
            let decision = self.decide(
                generation,
                request,
                at,
                entity,
                view,
                owner_view.as_deref(),
                scoped_families,
            )?;
            decided.insert(at, decision);
        }

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
                    let terms = self.resolve_terms(&descriptors);
                    if terms.len() > bound as usize {
                        over_bound.push(at);
                    }
                    keys.extend(identities[at].unique.iter().copied());
                    slots.push(Slot::Written(rows.len() as u32));
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
                        terms,
                    });
                }
                Some(entity) => match decided.remove(&at).expect("every named row was decided") {
                    Decided::Unchanged => slots.push(Slot::Unchanged(*entity)),
                    Decided::Added(join) => {
                        slots.push(Slot::Written(rows.len() as u32));
                        written_from.push(at);
                        rows.push(*join);
                    }
                    Decided::Changes(what) => {
                        let tid = self.issued_tessera_id(generation, *entity);
                        return Err(AcceptError::Conflict(format!(
                            "row {at} would change item {tid}: {what}; editing an item is not \
                             available yet, so send rows that create items, add them to a view \
                             or carry what is stored"
                        )));
                    }
                },
            }
        }

        let artifacts = Self::written_memberships(request, &written_from);
        Ok(Planned {
            rows,
            slots,
            keys,
            artifacts,
            over_bound,
        })
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
    ) -> Result<Decided, AcceptError> {
        let row = &request.rows[at];
        let manifest = &generation.bundle.manifest;
        let declared = &manifest.declared_scalars;
        let buffered = generation.buffer.get(entity);
        let pending = generation.buffer.fill_of(entity);
        let mut blob = BlobRow::default();
        let carried = |position: usize| !row.omitted.contains(&position);

        if let Some(labels) = &row.labels {
            let descriptors = self
                .plugin
                .terms_of_labels(labels)
                .map_err(|e| AcceptError::Contract(format!("row {at}, access: {e}")))?;
            let mut supplied: Vec<TermId> = self.resolve_terms(&descriptors);
            let held: Option<Vec<TermId>> = match buffered {
                Some(item) => Some(item.terms.clone()),
                None => joined::flushed_terms_of(generation, entity),
            };
            supplied.sort_unstable();
            supplied.dedup();
            let same = held.is_some_and(|mut held| {
                held.sort_unstable();
                held.dedup();
                held == supplied
            });
            if !same {
                return Ok(Decided::Changes("the label".to_string()));
            }
        }

        if let Some(external_id) = &row.external_id {
            let held = self.stored_external_id(generation, entity)?;
            if held.as_deref() != Some(external_id.as_slice()) {
                return Ok(Decided::Changes("the external_id".to_string()));
            }
        }

        // Each carried value against the stored one; a null is compared as no value.
        for (position, d) in declared.iter().enumerate() {
            let Some(value) = row.scalars.get(position).filter(|_| carried(position)) else {
                continue;
            };
            let value = (!joined::scalar_is_absent(value, d)).then_some(value);
            let held = joined::held_entity_value(
                generation, entity, position, d, buffered, pending, &mut blob,
            );
            if value != held.as_ref() {
                return Ok(Decided::Changes(format!("'{}'", d.name)));
            }
        }

        let (Some(view), Some((x, y))) = (view, row.position) else {
            // No position: nothing about the item's views changes.
            if let Some(owner_view) = owner_view {
                if let Err(what) =
                    scoped_against_held(generation, entity, row, scoped_families, owner_view, false)
                {
                    return Ok(Decided::Changes(what));
                }
            }
            return Ok(self.membership_decision(request, at, entity));
        };
        let owner_view = owner_view.expect("a view has an owner view");
        let adding = match self.stored_position(generation, entity, view)? {
            Some(held) => {
                if held != fixed(view.quantisation, x, y) {
                    return Ok(Decided::Changes(format!(
                        "the position in view '{}'",
                        view.id
                    )));
                }
                false
            }
            None => true,
        };
        let repeated =
            match scoped_against_held(generation, entity, row, scoped_families, owner_view, adding)
            {
                Ok(repeated) => repeated,
                Err(what) => return Ok(Decided::Changes(what)),
            };
        if !adding {
            return Ok(self.membership_decision(request, at, entity));
        }
        if !joined::joins_in_place(generation, entity, &view.id) {
            return Ok(Decided::Changes(format!(
                "adding it to view '{}', which holds newer items, moves it",
                view.id
            )));
        }
        // The row a view gains carries the item's values. A rendered value is read from the item
        // where the row leaves it out, so the view renders what the item's other views do.
        let scalars: Vec<WalScalar> = declared
            .iter()
            .enumerate()
            .map(|(position, d)| match row.scalars.get(position) {
                Some(value) if carried(position) => value.clone(),
                _ if d.render => joined::held_entity_value(
                    generation, entity, position, d, buffered, pending, &mut blob,
                )
                .unwrap_or(WalScalar::Null),
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

    /// The row's membership columns, for a row writing nothing: each artifact named must already
    /// hold the item, or the row would change what the item belongs to.
    fn membership_decision(&self, request: &IngestRequest, at: usize, entity: EntityId) -> Decided {
        let Ok(member) = u32::try_from(entity.raw()) else {
            return Decided::Changes("the memberships".to_string());
        };
        for membership in &request.artifacts.memberships {
            if !membership.rows.contains(&(at as u32)) {
                continue;
            }
            let held = self.write.live().with_artifacts(|store| {
                store
                    .ordinal_of_key(
                        &membership.layer,
                        membership.level,
                        membership.view.as_deref(),
                        &membership.key,
                    )
                    .and_then(|ordinal| store.get(&membership.layer, membership.level, ordinal))
                    .is_some_and(|record| store.members_of(record).contains(member))
            });
            if !held {
                return Decided::Changes(format!(
                    "the memberships of layer '{}'",
                    membership.layer
                ));
            }
        }
        Decided::Unchanged
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
            .map_err(|e| AcceptError::UniqueIndexUnreadable(e.to_string()))
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
                .map_err(|e| AcceptError::UniqueIndexUnreadable(e.to_string()))?;
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
    /// each written row came from.
    fn written_memberships(request: &IngestRequest, written_from: &[usize]) -> BatchArtifacts {
        let position: BTreeMap<u32, u32> = written_from
            .iter()
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
            crate::unique::tessera_id_text(&self.identity_key, generation, entity)
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

/// What one row naming an item does to it.
enum Decided {
    Unchanged,
    /// The row adds the item to the batch's view, as this row.
    Added(Box<UnallocatedRow>),
    /// The row would change what this names.
    Changes(String),
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
/// value the row repeats, or the first that differs, as a refusal names it. An empty cell takes a
/// value where the row adds the item to the view (`adding`), and differs from one otherwise. A
/// cell holding prose that a flush has made uncomparable differs from any value.
fn scoped_against_held(
    generation: &Generation,
    entity: EntityId,
    row: &IngestRow,
    families: &[tessera_store::manifest::ScopedScalar],
    owner_view: &str,
    adding: bool,
) -> Result<Vec<usize>, String> {
    let declared_len = generation.bundle.manifest.declared_scalars.len();
    let pending = generation.buffer.scoped_fill_of(entity, owner_view);
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
        let held = joined::held_scoped_value(
            generation, entity, position, family, &d, owner_view, pending,
        );
        let prose = held.is_none()
            && family.arrow_type == tessera_spatial::tiler::ScalarType::Text
            && joined::flushed_scoped_text_present(generation, entity, family, owner_view);
        match (supplied, held) {
            (None, None) if !prose => {}
            (Some(_), None) if adding && !prose => {}
            (Some(value), Some(held)) if *value == held => repeated.push(position),
            _ => return Err(format!("'{}' in view '{owner_view}'", family.name)),
        }
    }
    Ok(repeated)
}

/// Who holds what in one generation: the unique indexes and their live entries, the external ids
/// bound, and the `tessera_id`s issued, deleted items left out of each.
struct Held<'a> {
    engine: &'a Engine,
    generation: &'a Generation,
    declared: &'a [DeclaredScalar],
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
            .map_err(|e| AcceptError::UniqueIndexUnreadable(e.to_string()))?;
        let mut out = vec![Vec::new(); keys.len()];
        for (at, entity) in found {
            out[at].push(entity);
        }
        Ok(out)
    }

    fn external_holders(&self, ids: &[&[u8]]) -> Result<Vec<Option<EntityId>>, AcceptError> {
        let owned: Vec<Vec<u8>> = ids.iter().map(|id| id.to_vec()).collect();
        let found = self
            .engine
            .resolve_external_ids(&owned)
            .map_err(|e| AcceptError::UniqueIndexUnreadable(e.to_string()))?;
        Ok(found
            .into_iter()
            .map(|entity| entity.filter(|e| !self.generation.overlay.is_deleted(*e)))
            .collect())
    }

    fn tessera_holders(&self, ids: &[TesseraId]) -> Vec<Option<EntityId>> {
        let shard = self.generation.bundle.manifest.identity.shard_id;
        let high_water = self.engine.allocator_high_water();
        ids.iter()
            .map(|id| {
                let (id_shard, entity) = self.engine.identity_key.invert(*id);
                // A deletion a fold has retired is no longer in the overlay; the item is gone
                // from every row space too, and an item always has a row or a buffered one.
                let held = id_shard == shard
                    && entity.raw() < high_water
                    && !self.generation.overlay.is_deleted(entity)
                    && (self.generation.buffer.contains(entity)
                        || self.generation.bundle.partitions.values().any(|partition| {
                            partition
                                .views
                                .values()
                                .any(|view| view.row_space.row_of(entity).is_some())
                        }));
                held.then_some(entity)
            })
            .collect()
    }
}
