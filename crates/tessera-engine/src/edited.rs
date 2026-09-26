//! Edited items at a running service: the entries of the edited-items map no run holds yet, and
//! the one translation between an item's `tessera_id` and the entity holding it.
//!
//! An edit gives its item a new entity and keeps the item's number, the entity it was first given,
//! which its `tessera_id` is taken from ([`tessera_store::edited`]). The window that commits an
//! edit adds its pair here, and the first flush that gives the new entity a row writes a run
//! holding it, whose publication removes the pair from here. Every lookup reads both.
//!
//! A `tessera_id` names the entity [`entities_of_numbers`] answers: an entity of the number's
//! entries that the generation holds and has not deleted, or, where the number has no entries,
//! the number itself.

use rustc_hash::FxHashMap;
use tessera_lifecycle::WalRecord;
use tessera_store::StoreError;
use tessera_types::{EntityId, TesseraId};

use crate::Generation;

/// The edited items whose pair no run holds yet: number to entities, and entity to number.
#[derive(Debug, Clone, Default)]
pub(crate) struct EditedLive {
    by_number: FxHashMap<u32, Vec<u32>>,
    by_entity: FxHashMap<u32, u32>,
}

impl EditedLive {
    /// The pairs of the edits `records` carry whose new entity `buffer` still holds a row of:
    /// what a restart starts from.
    pub(crate) fn derive(
        records: &[WalRecord],
        buffer: &tessera_lifecycle::IngestBuffer,
    ) -> EditedLive {
        let mut live = EditedLive::default();
        let pairs: Vec<(u32, u32)> = records
            .iter()
            .filter_map(|record| match record {
                WalRecord::IngestBatch { edits, .. } => Some(edits),
                _ => None,
            })
            .flatten()
            .filter_map(|edit| {
                let entity = edit.rows.first()?.entity_id;
                buffer
                    .contains(entity)
                    .then(|| (narrow(edit.number), narrow(entity)))
            })
            .collect();
        live.add(&pairs);
        live
    }

    /// Add `(number, entity)` pairs.
    pub(crate) fn add(&mut self, pairs: &[(u32, u32)]) {
        for &(number, entity) in pairs {
            let held = self.by_number.entry(number).or_default();
            if !held.contains(&entity) {
                held.push(entity);
            }
            self.by_entity.insert(entity, number);
        }
    }

    /// Remove the pairs of `entities`: a flush has written them into a run, or the entities are
    /// deleted and name nothing.
    pub(crate) fn remove(&mut self, entities: impl IntoIterator<Item = u32>) {
        for entity in entities {
            let Some(number) = self.by_entity.remove(&entity) else {
                continue;
            };
            if let Some(held) = self.by_number.get_mut(&number) {
                held.retain(|e| *e != entity);
                if held.is_empty() {
                    self.by_number.remove(&number);
                }
            }
        }
    }

    /// The number `entity` holds, where its pair is here.
    pub(crate) fn number_of(&self, entity: u32) -> Option<u32> {
        self.by_entity.get(&entity).copied()
    }
}

fn narrow(entity: EntityId) -> u32 {
    u32::try_from(entity.raw()).expect("entity ids are capped at u32::MAX by I9")
}

/// Each entity's number: the one its pair names, or the entity itself.
pub(crate) fn numbers_of(
    generation: &Generation,
    entities: &[EntityId],
) -> Result<Vec<EntityId>, StoreError> {
    let raw: Vec<u32> = entities.iter().map(|e| narrow(*e)).collect();
    let mut out: Vec<EntityId> = entities.to_vec();
    let mut unresolved: Vec<usize> = Vec::new();
    for (at, entity) in raw.iter().enumerate() {
        match generation.edited_live.number_of(*entity) {
            Some(number) => out[at] = EntityId::new(u64::from(number)),
            None => unresolved.push(at),
        }
    }
    if unresolved.is_empty() {
        return Ok(out);
    }
    let asked: Vec<u32> = unresolved.iter().map(|&at| raw[at]).collect();
    for (at, number) in generation.edited.numbers_of(&asked)? {
        out[unresolved[at]] = EntityId::new(u64::from(number));
    }
    Ok(out)
}

/// For each number, every entity its pairs name, in the runs and here, in ascending order.
pub(crate) fn entries_of(
    generation: &Generation,
    numbers: &[u32],
) -> Result<Vec<Vec<EntityId>>, StoreError> {
    let mut out: Vec<Vec<EntityId>> = vec![Vec::new(); numbers.len()];
    for (at, number) in numbers.iter().enumerate() {
        if let Some(held) = generation.edited_live.by_number.get(number) {
            out[at].extend(held.iter().map(|e| EntityId::new(u64::from(*e))));
        }
    }
    for (at, entity) in generation.edited.entities_of(numbers)? {
        out[at].push(EntityId::new(u64::from(entity)));
    }
    for entities in &mut out {
        entities.sort_unstable();
        entities.dedup();
    }
    Ok(out)
}

/// The entity holding each item `numbers` names: of the number's entries, the one `holds` answers
/// for and the overlay has not deleted; where the number has no entries, the number itself where
/// `holds` answers for it. `None` for a number naming nothing.
pub(crate) fn entities_of_numbers(
    generation: &Generation,
    numbers: &[EntityId],
    holds: impl Fn(EntityId) -> bool,
) -> Result<Vec<Option<EntityId>>, StoreError> {
    let raw: Vec<u32> = numbers.iter().map(|n| narrow(*n)).collect();
    let entries = entries_of(generation, &raw)?;
    Ok(numbers
        .iter()
        .zip(entries)
        .map(|(number, entries)| {
            if entries.is_empty() {
                return holds(*number).then_some(*number);
            }
            entries
                .into_iter()
                .rev()
                .find(|e| !generation.overlay.is_deleted(*e) && holds(*e))
        })
        .collect())
}

impl crate::Engine {
    /// The `tessera_id` of each of `entities`, against `generation`: its number's permutation.
    pub(crate) fn tessera_ids_of_in(
        &self,
        generation: &Generation,
        entities: &[EntityId],
    ) -> Result<Vec<TesseraId>, StoreError> {
        let shard = generation.bundle.manifest.identity.shard_id;
        numbers_of(generation, entities)?
            .into_iter()
            .map(|number| {
                self.identity_key
                    .forward(shard, number)
                    .map_err(|e| StoreError::MalformedBundle {
                        detail: format!("an issued entity lies outside the identity space: {e}"),
                    })
            })
            .collect()
    }

    /// [`Self::tessera_ids_of_in`] for one entity.
    pub(crate) fn tessera_id_in(
        &self,
        generation: &Generation,
        entity: EntityId,
    ) -> Result<TesseraId, StoreError> {
        Ok(self.tessera_ids_of_in(generation, &[entity])?[0])
    }
}
