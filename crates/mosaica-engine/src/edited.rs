//! Edited items at a running service: the entries of the edited-items map no run holds yet, and
//! the one translation between an item's `mosaica_id` and the entity holding it.
//!
//! An edit gives its item a new entity and keeps the item's number, the entity it was first given,
//! which its `mosaica_id` is taken from ([`mosaica_store::edited`]). The window that commits an
//! edit adds its pair here, and the first flush that gives the new entity a row writes a run
//! holding it, whose publication removes the pair from here. A new entity deleted or edited away
//! before that flush keeps its pair here until the fold that removes the entity. Every lookup reads
//! both.
//!
//! A `mosaica_id` names the entity [`entities_of_numbers`] answers: an entity of the number's
//! entries that the generation holds and has not deleted, or, where the number has no entries,
//! the number itself.
//!
//! A number's `mosaica_id` carries the number's tenancy, which the generation's tenancy index holds
//! ([`mosaica_store::tenancy`]). An identifier is formed at the tenancy the index holds for its
//! number, and names the number only while the index still holds that tenancy for it. An artifact
//! or a layer is an entity that is never freed, so its identifier is at tenancy 0 and the index is
//! not read for it.

use mosaica_store::StoreError;
use mosaica_types::{EntityId, IdentityError, IdentityKey, ItemHigh, MosaicaId, Tenancy};
use rustc_hash::FxHashMap;

use crate::Generation;

/// The edited items whose pair no run holds yet: number to entities, and entity to number.
#[derive(Debug, Clone, Default)]
pub(crate) struct EditedLive {
    by_number: FxHashMap<u32, Vec<u32>>,
    by_entity: FxHashMap<u32, u32>,
}

impl EditedLive {
    /// The `(number, new entity)` pairs of the replayed edits whose new entity `buffer` still
    /// holds a row of, or `overlay` deletes: what a restart starts from.
    ///
    /// The caller passes only the edits whose first row no flush has written. A flushed edit's
    /// pair is in a run, so an entity flushed and then deleted keeps no pair here.
    pub(crate) fn derive(
        edits: &[(EntityId, EntityId)],
        buffer: &mosaica_lifecycle::IngestBuffer,
        overlay: &mosaica_lifecycle::Overlay,
    ) -> EditedLive {
        let mut live = EditedLive::default();
        let pairs: Vec<(u32, u32)> = edits
            .iter()
            .filter(|(_, entity)| buffer.contains(*entity) || overlay.is_deleted(*entity))
            .map(|&(number, entity)| (narrow(number), narrow(entity)))
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

    /// Remove the pairs of `entities`: a flush has written them into a run, or a fold has removed
    /// the entities.
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

/// Whether `generation` holds a row of `entity`, flushed or buffered.
pub(crate) fn holds(generation: &Generation, entity: EntityId) -> bool {
    generation.buffer.contains(entity)
        || generation.bundle.partitions.values().any(|partition| {
            partition
                .views
                .values()
                .any(|data| data.row_space.row_of(entity).is_some())
        })
}

/// The entity holding the item `entity` held, where an edit has moved the item since; `None`
/// where it has not moved or its item is gone.
pub(crate) fn moved_to(
    generation: &Generation,
    entity: EntityId,
) -> Result<Option<EntityId>, StoreError> {
    let number = numbers_of(generation, &[entity])?[0];
    let now = entities_of_numbers(generation, &[number], |e| holds(generation, e))?[0];
    Ok(now.filter(|now| *now != entity))
}

/// Each member of `set` an edit has moved, replaced by the entity its item holds now. Only a
/// member the overlay deletes can have moved: an edit deletes the entity it moves away from.
pub(crate) fn follow_moves(
    generation: &Generation,
    set: &mut croaring::Bitmap,
) -> Result<(), StoreError> {
    let deleted = set.and(generation.overlay.deleted_set());
    for old in deleted.iter() {
        if let Some(now) = moved_to(generation, EntityId::new(u64::from(old)))? {
            set.remove(old);
            set.add(narrow(now));
        }
    }
    Ok(())
}

/// `changes` with each change naming an entity an edit has moved an item away from also applied
/// to the entity the item holds now. The change to the old entity stays, so the log names both.
pub(crate) fn follow_changes(
    generation: &Generation,
    changes: &mut Vec<(EntityId, mosaica_lifecycle::ChangeOp)>,
) -> Result<(), StoreError> {
    let mut followed = Vec::new();
    for &(entity, op) in changes.iter() {
        if !generation.overlay.is_deleted(entity) && holds(generation, entity) {
            continue;
        }
        if let Some(now) = moved_to(generation, entity)? {
            followed.push((now, op));
        }
    }
    changes.extend(followed);
    Ok(())
}

/// Every entity set of `artifacts`, members and generating sets, following moves.
pub(crate) fn follow_artifacts(
    generation: &Generation,
    artifacts: &mut [mosaica_lifecycle::IncomingArtifact],
) -> Result<(), StoreError> {
    for artifact in artifacts {
        follow_moves(generation, &mut artifact.members)?;
        if let Some(excluding) = artifact.excluding.as_mut() {
            follow_moves(generation, excluding)?;
        }
        for content in &mut artifact.contents {
            follow_moves(generation, &mut content.generated_from)?;
        }
    }
    Ok(())
}

/// Every entity set of `joins`, following moves.
pub(crate) fn follow_growth(
    generation: &Generation,
    joins: &mut [mosaica_lifecycle::IncomingGrowth],
) -> Result<(), StoreError> {
    for join in joins {
        follow_moves(generation, &mut join.joining)?;
        follow_moves(generation, &mut join.leaving)?;
    }
    Ok(())
}

/// The epochs a command's entities were resolved at: a later generation that committed an edit
/// may have moved one, and one that also retired entities in a fold may have dropped the map
/// entry that says where to.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub(crate) struct Stamp {
    edits: u64,
    folds: u64,
}

/// What has happened to a command's entities since its [`Stamp`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Since {
    /// No edit committed: every entity is where it was.
    Still,
    /// An edit committed: [`follow_moves`] finds where each moved entity's item is now.
    Edited,
    /// An edit committed and a fold retired entities: the names must be resolved again.
    Folded,
}

impl Stamp {
    pub(crate) fn of(generation: &Generation) -> Stamp {
        Stamp {
            edits: generation.edit_epoch,
            folds: generation.fold_epoch,
        }
    }

    pub(crate) fn since(self, generation: &Generation) -> Since {
        match (
            generation.edit_epoch == self.edits,
            generation.fold_epoch == self.folds,
        ) {
            (true, _) => Since::Still,
            (false, true) => Since::Edited,
            (false, false) => Since::Folded,
        }
    }
}

/// The number `id` names in `generation`'s shard, or `None` where `id` names no item, an item of
/// another shard, or its number at a tenancy the number is not held at.
pub(crate) fn number_named(
    key: &IdentityKey,
    generation: &Generation,
    id: MosaicaId,
) -> Option<EntityId> {
    let (number, current) = number_and_tenancy_named(key, generation, id)?;
    current.then_some(number)
}

/// The number `id` names in `generation`'s shard, and whether `id` carries the tenancy the index
/// holds for that number; `None` where `id` names no item or an item of another shard. The index
/// is read for every identifier that reaches it.
pub(crate) fn number_and_tenancy_named(
    key: &IdentityKey,
    generation: &Generation,
    id: MosaicaId,
) -> Option<(EntityId, bool)> {
    let (high, number) = key.invert(id)?;
    let shard = generation.bundle.manifest.identity.shard_id;
    (high.shard == shard).then(|| (number, generation.tenancy.of(number) == high.tenancy))
}

/// The `mosaica_id` of the item holding each of `numbers` as its number, at the tenancy the index
/// holds for it.
pub(crate) fn ids_of_numbers(
    key: &IdentityKey,
    generation: &Generation,
    numbers: &[EntityId],
) -> Result<Vec<MosaicaId>, StoreError> {
    let shard = generation.bundle.manifest.identity.shard_id;
    let tenancies = generation.tenancy.of_each(numbers.iter().copied());
    numbers
        .iter()
        .zip(tenancies)
        .map(|(number, tenancy)| {
            key.forward(ItemHigh::new(shard, tenancy), *number)
                .map_err(|e| StoreError::MalformedBundle {
                    detail: format!("an issued entity lies outside the identity space: {e}"),
                })
        })
        .collect()
}

/// The `mosaica_id` of the artifact or layer `entity` in `shard`.
pub(crate) fn artifact_id(
    key: &IdentityKey,
    shard: u32,
    entity: EntityId,
) -> Result<MosaicaId, IdentityError> {
    key.forward(ItemHigh::new(shard, Tenancy::ZERO), entity)
}

/// The entity `id` names in `shard` as [`artifact_id`] forms it, or `None` where it names none.
/// Whether that entity is an artifact is the caller's question.
pub(crate) fn artifact_named(key: &IdentityKey, shard: u32, id: MosaicaId) -> Option<EntityId> {
    let (high, entity) = key.invert(id)?;
    (high == ItemHigh::new(shard, Tenancy::ZERO)).then_some(entity)
}

impl crate::Engine {
    /// The `mosaica_id` of each of `entities`, against `generation`: its number's permutation.
    pub(crate) fn mosaica_ids_of_in(
        &self,
        generation: &Generation,
        entities: &[EntityId],
    ) -> Result<Vec<MosaicaId>, StoreError> {
        ids_of_numbers(
            &self.identity_key,
            generation,
            &numbers_of(generation, entities)?,
        )
    }

    /// [`Self::mosaica_ids_of_in`] for one entity.
    pub(crate) fn mosaica_id_in(
        &self,
        generation: &Generation,
        entity: EntityId,
    ) -> Result<MosaicaId, StoreError> {
        Ok(self.mosaica_ids_of_in(generation, &[entity])?[0])
    }
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use mosaica_lifecycle::{IngestBuffer, Overlay};
    use mosaica_store::tenancy::{TenancyIndex, TENANCY_BITS};

    use super::*;

    const KEY: &str = "000102030405060708090a0b0c0d0e0f";

    /// A generation of shard 0 whose index holds each `(number, tenancy)`.
    fn generation_with(dir: &std::path::Path, held: &[(u32, u16)]) -> Generation {
        let mut bits: [croaring::Bitmap; TENANCY_BITS] = Default::default();
        for &(number, tenancy) in held {
            for (bit, numbers) in bits.iter_mut().enumerate() {
                if tenancy & (1 << bit) != 0 {
                    numbers.add(number);
                }
            }
        }
        let files = mosaica_store::tenancy::write(dir, &bits).unwrap();
        let index = Arc::new(TenancyIndex::open(&files).unwrap());
        Generation::synthetic("v00000", 1, 0, Overlay::new(), IngestBuffer::new())
            .with(|g| g.tenancy = index)
    }

    fn id(key: &IdentityKey, shard: u32, tenancy: u16, number: u64) -> MosaicaId {
        let high = ItemHigh::new(shard, Tenancy::new(tenancy).unwrap());
        key.forward(high, EntityId::new(number)).unwrap()
    }

    #[test]
    fn an_identifier_names_its_number_only_at_the_tenancy_the_index_holds() {
        let dir = tempfile::tempdir().unwrap();
        let generation = generation_with(dir.path(), &[(7, 3), (9, 4095)]);
        let key = IdentityKey::from_hex(KEY).unwrap();
        let named = |id| number_named(&key, &generation, id);

        assert_eq!(named(id(&key, 0, 3, 7)), Some(EntityId::new(7)));
        assert_eq!(
            named(id(&key, 0, 0, 7)),
            None,
            "an earlier holder's identifier"
        );
        assert_eq!(named(id(&key, 0, 4, 7)), None, "a tenancy not yet reached");
        assert_eq!(named(id(&key, 0, 4095, 9)), Some(EntityId::new(9)));
        assert_eq!(
            named(id(&key, 0, 0, 8)),
            Some(EntityId::new(8)),
            "never freed"
        );
        assert_eq!(named(id(&key, 1, 3, 7)), None, "another shard's");
        assert_eq!(
            number_and_tenancy_named(&key, &generation, id(&key, 0, 2, 7)),
            Some((EntityId::new(7), false))
        );
    }

    #[test]
    fn an_identifier_is_formed_at_the_tenancy_the_index_holds() {
        let dir = tempfile::tempdir().unwrap();
        let generation = generation_with(dir.path(), &[(7, 3)]);
        let key = IdentityKey::from_hex(KEY).unwrap();
        let numbers = numbers_of(&generation, &[EntityId::new(7), EntityId::new(8)]).unwrap();
        let ids = ids_of_numbers(&key, &generation, &numbers).unwrap();
        assert_eq!(ids, [id(&key, 0, 3, 7), id(&key, 0, 0, 8)]);
        assert!(ids
            .iter()
            .all(|id| number_named(&key, &generation, *id).is_some()));
    }
}
