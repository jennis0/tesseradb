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
//! A number's `mosaica_id` carries the number's tenancy, which its shard's tenancy index holds
//! ([`mosaica_store::tenancy`]). An identifier is formed at the tenancy the index holds for its
//! number, and names the number only while the index still holds that tenancy for it. An artifact
//! or a layer is an entity that is never freed, so its identifier is at tenancy 0 and the index is
//! not read for it.

use mosaica_store::StoreError;
use mosaica_types::{
    EntityId, IdentityError, IdentityKey, ItemHigh, LayerSpace, MosaicaId, ShardId, Target, Tenancy,
};
use rustc_hash::FxHashMap;

use crate::{Generation, Shard};

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

/// Each entity's number in `shard`: the one its pair names, or the entity itself.
pub(crate) fn numbers_of(
    shard: &Shard,
    entities: &[EntityId],
) -> Result<Vec<EntityId>, StoreError> {
    let raw: Vec<u32> = entities.iter().map(|e| narrow(*e)).collect();
    let mut out: Vec<EntityId> = entities.to_vec();
    let mut unresolved: Vec<usize> = Vec::new();
    for (at, entity) in raw.iter().enumerate() {
        match shard.edited_live.number_of(*entity) {
            Some(number) => out[at] = EntityId::new(u64::from(number)),
            None => unresolved.push(at),
        }
    }
    if unresolved.is_empty() {
        return Ok(out);
    }
    let asked: Vec<u32> = unresolved.iter().map(|&at| raw[at]).collect();
    for (at, number) in shard.edited.numbers_of(&asked)? {
        out[unresolved[at]] = EntityId::new(u64::from(number));
    }
    Ok(out)
}

/// For each number, every entity its pairs name, in the runs and here, in ascending order.
pub(crate) fn entries_of(shard: &Shard, numbers: &[u32]) -> Result<Vec<Vec<EntityId>>, StoreError> {
    let mut out: Vec<Vec<EntityId>> = vec![Vec::new(); numbers.len()];
    for (at, number) in numbers.iter().enumerate() {
        if let Some(held) = shard.edited_live.by_number.get(number) {
            out[at].extend(held.iter().map(|e| EntityId::new(u64::from(*e))));
        }
    }
    for (at, entity) in shard.edited.entities_of(numbers)? {
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
    shard: &Shard,
    numbers: &[EntityId],
    holds: impl Fn(EntityId) -> bool,
) -> Result<Vec<Option<EntityId>>, StoreError> {
    let raw: Vec<u32> = numbers.iter().map(|n| narrow(*n)).collect();
    let entries = entries_of(shard, &raw)?;
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
                .find(|e| !shard.overlay.is_deleted(*e) && holds(*e))
        })
        .collect())
}

/// Whether `shard` holds a row of `entity`, flushed or buffered.
pub(crate) fn holds(shard: &Shard, entity: EntityId) -> bool {
    shard.buffer.contains(entity)
        || shard
            .data
            .views
            .values()
            .any(|data| data.row_space.row_of(entity).is_some())
}

/// Whether `shard` holds a row of `entity` in `view`, flushed or buffered. A buffered row is in no
/// permutation, and missing it would let two batches hand one flush two rows for one entity in one
/// view.
pub(crate) fn holds_in_view(shard: &Shard, entity: EntityId, view: &str) -> bool {
    shard
        .data
        .views
        .get(view)
        .is_some_and(|data| data.row_space.row_of(entity).is_some())
        || shard.buffer.contains_in_view(entity, view)
}

/// The entity holding the item `entity` held, where an edit has moved the item since; `None`
/// where it has not moved or its item is gone.
pub(crate) fn moved_to(shard: &Shard, entity: EntityId) -> Result<Option<EntityId>, StoreError> {
    let number = numbers_of(shard, &[entity])?[0];
    let now = entities_of_numbers(shard, &[number], |e| holds(shard, e))?[0];
    Ok(now.filter(|now| *now != entity))
}

/// Each member of `set` an edit has moved, replaced by the entity its item holds now. Only a
/// member the overlay deletes can have moved: an edit deletes the entity it moves away from.
pub(crate) fn follow_moves(shard: &Shard, set: &mut croaring::Bitmap) -> Result<(), StoreError> {
    let deleted = set.and(shard.overlay.deleted_set());
    for old in deleted.iter() {
        if let Some(now) = moved_to(shard, EntityId::new(u64::from(old)))? {
            set.remove(old);
            set.add(narrow(now));
        }
    }
    Ok(())
}

/// `changes` with each change naming an entity an edit has moved an item away from also applied
/// to the entity the item holds now, in the item's shard of `generation`. The change to the old
/// entity stays, so the log names both. An artifact is never moved.
pub(crate) fn follow_changes(
    generation: &Generation,
    changes: &mut Vec<(Target, mosaica_lifecycle::ChangeOp)>,
) -> Result<(), StoreError> {
    let mut followed = Vec::new();
    for &(target, op) in changes.iter() {
        let Target::Item(at, entity) = target else {
            continue;
        };
        let Some(shard) = generation.shard(at) else {
            continue;
        };
        if !shard.overlay.is_deleted(entity) && holds(shard, entity) {
            continue;
        }
        if let Some(now) = moved_to(shard, entity)? {
            followed.push((Target::Item(at, now), op));
        }
    }
    changes.extend(followed);
    Ok(())
}

/// Every set of `sets` following moves, each in its own shard of `generation`.
fn follow_sets(
    generation: &Generation,
    sets: &mut mosaica_lifecycle::ShardSets,
) -> Result<(), StoreError> {
    sets.try_for_each_mut(|id, set| match generation.shard(id) {
        Some(shard) => follow_moves(shard, set),
        None => Ok(()),
    })
}

/// Every entity set of `artifacts`, members and generating sets, following moves.
pub(crate) fn follow_artifacts(
    generation: &Generation,
    artifacts: &mut [mosaica_lifecycle::IncomingArtifact],
) -> Result<(), StoreError> {
    for artifact in artifacts {
        follow_sets(generation, &mut artifact.members)?;
        if let Some(excluding) = artifact.excluding.as_mut() {
            follow_sets(generation, excluding)?;
        }
        for content in &mut artifact.contents {
            follow_sets(generation, &mut content.generated_from)?;
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
        follow_sets(generation, &mut join.joining)?;
        follow_sets(generation, &mut join.leaving)?;
    }
    Ok(())
}

/// The generation a write's entities were resolved against, which [`crate::Engine::name_items`]
/// answers with its names and the write carries to the executor. [`Since`] is what a later
/// generation can have done to those entities, and whether the write's names still hold.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[cfg_attr(test, derive(Default))]
pub struct Stamp {
    edits: u64,
    folds: u64,
    frees: u64,
}

/// What has happened to a write's entities since its [`Stamp`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Since {
    /// No edit committed and no fold freed an id: every entity is where it was.
    Still,
    /// An edit committed and no fold retired an entity. An edit moves its item to a new entity,
    /// and [`follow_moves`] finds where each moved entity's item is now.
    Edited,
    /// A fold freed ids, and may have issued one the write names to another item. Or an edit
    /// committed and a fold retired entities, which can drop the map entry saying where an item
    /// went. The names are resolved again.
    Folded,
}

impl Stamp {
    pub(crate) fn of(generation: &Generation) -> Stamp {
        Stamp {
            edits: generation.edit_epoch,
            folds: generation.fold_epoch,
            frees: generation.free_epoch,
        }
    }

    fn since(self, generation: &Generation) -> Since {
        if generation.free_epoch != self.frees {
            return Since::Folded;
        }
        match (
            generation.edit_epoch == self.edits,
            generation.fold_epoch == self.folds,
        ) {
            (true, _) => Since::Still,
            (false, true) => Since::Edited,
            (false, false) => Since::Folded,
        }
    }

    /// Whether a write resolved at this stamp names its entities where their items are in
    /// `generation`, `follow` having moved each that an edit has moved since. A write's entities
    /// are followed here and nowhere else, so each is followed once, from the generation it was
    /// resolved against. `false` where the names are stale by [`Since`]'s rule or `follow` cannot
    /// read the edited items: the caller resolves the write's names again.
    pub(crate) fn follow_since(
        self,
        generation: &Generation,
        follow: impl FnOnce(&Generation) -> Result<(), StoreError>,
    ) -> bool {
        match self.since(generation) {
            Since::Still => true,
            Since::Edited => follow(generation).is_ok(),
            Since::Folded => false,
        }
    }
}

/// The shard of `generation` whose item `id` names, the number it names there, and whether `id`
/// carries the tenancy that shard's index holds for the number. `None` where `id` names no item or
/// an item of a shard `generation` does not hold. The index is read for every identifier of a shard
/// it holds.
pub(crate) fn named<'g>(
    key: &IdentityKey,
    generation: &'g Generation,
    id: MosaicaId,
) -> Option<(&'g Shard, EntityId, bool)> {
    let (high, number) = key.invert(id)?;
    let shard = generation.shard(high.shard)?;
    Some((shard, number, shard.tenancy.of(number) == high.tenancy))
}

/// The shard and number `id` names, or `None` where it names no item, a shard `generation` does
/// not hold, or a number at a tenancy the index does not hold.
pub(crate) fn number_named<'g>(
    key: &IdentityKey,
    generation: &'g Generation,
    id: MosaicaId,
) -> Option<(&'g Shard, EntityId)> {
    let (shard, number, current) = named(key, generation, id)?;
    current.then_some((shard, number))
}

/// The `mosaica_id` of the item holding each of `numbers` as its number in `shard`, at the tenancy
/// the shard's index holds for it.
pub(crate) fn ids_of_numbers(
    key: &IdentityKey,
    shard: &Shard,
    numbers: &[EntityId],
) -> Result<Vec<MosaicaId>, StoreError> {
    let tenancies = shard.tenancy.of_each(numbers.iter().copied());
    numbers
        .iter()
        .zip(tenancies)
        .map(|(number, tenancy)| {
            id_at(key, shard.id(), tenancy, *number).map_err(|e| StoreError::MalformedBundle {
                detail: format!("an issued entity lies outside the identity space: {e}"),
            })
        })
        .collect()
}

/// The `mosaica_id` of the item holding `number` at `tenancy` in `shard`.
pub(crate) fn id_at(
    key: &IdentityKey,
    shard: ShardId,
    tenancy: Tenancy,
    number: EntityId,
) -> Result<MosaicaId, IdentityError> {
    key.forward(ItemHigh::new(shard, tenancy), number)
}

impl crate::Engine {
    /// The `mosaica_id` of the artifact or layer `entity` of layer space `space`, or `None` where
    /// `entity` lies outside the identity space.
    pub(crate) fn artifact_id(&self, space: LayerSpace, entity: EntityId) -> Option<MosaicaId> {
        self.identity_key.forward_artifact(space, entity).ok()
    }

    /// The `mosaica_id` of each of `entities` of `shard`: its number's permutation.
    pub(crate) fn mosaica_ids_of_in(
        &self,
        shard: &Shard,
        entities: &[EntityId],
    ) -> Result<Vec<MosaicaId>, StoreError> {
        ids_of_numbers(&self.identity_key, shard, &numbers_of(shard, entities)?)
    }

    /// [`Self::mosaica_ids_of_in`] for one entity.
    pub(crate) fn mosaica_id_in(
        &self,
        shard: &Shard,
        entity: EntityId,
    ) -> Result<MosaicaId, StoreError> {
        Ok(self.mosaica_ids_of_in(shard, &[entity])?[0])
    }
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use mosaica_lifecycle::{IngestBuffer, Overlay};
    use mosaica_store::tenancy::{bits_of, TenancyIndex};

    use super::*;

    const KEY: &str = "000102030405060708090a0b0c0d0e0f";

    /// A generation of shard 0 whose index holds each `(number, tenancy)`.
    fn generation_with(dir: &std::path::Path, held: &[(u32, u16)]) -> Generation {
        let files = mosaica_store::tenancy::write(dir, &bits_of(held)).unwrap();
        let index = Arc::new(TenancyIndex::open(&files).unwrap());
        let generation =
            Generation::synthetic("v00000", 1, 0, Overlay::new(), IngestBuffer::new());
        generation.with_shard(generation.only_shard().id(), |_, shard| shard.tenancy = index)
    }

    fn id(key: &IdentityKey, shard: u32, tenancy: u16, number: u64) -> MosaicaId {
        let high = ItemHigh::new(ShardId::new(shard).unwrap(), Tenancy::new(tenancy).unwrap());
        key.forward(high, EntityId::new(number)).unwrap()
    }

    #[test]
    fn an_identifier_names_its_number_only_at_the_tenancy_the_index_holds() {
        let dir = tempfile::tempdir().unwrap();
        let generation = generation_with(dir.path(), &[(7, 3), (9, 4095)]);
        let key = IdentityKey::from_hex(KEY).unwrap();
        let named = |id| number_named(&key, &generation, id).map(|(_, number)| number);

        assert_eq!(
            number_named(&key, &generation, id(&key, 0, 3, 7))
                .map(|(shard, number)| (shard.id(), number)),
            Some((ShardId::ZERO, EntityId::new(7))),
            "the number in its shard"
        );
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
        assert!(matches!(
            super::named(&key, &generation, id(&key, 0, 2, 7)),
            Some((shard, number, false))
                if shard.id() == generation.only_shard().id() && number == EntityId::new(7)
        ));
    }

    /// A write resolved at a stamp follows its entities after an edit alone, and is resolved
    /// again after a fold that freed ids, edit or none, or after an edit and a fold that retired
    /// entities.
    #[test]
    fn a_stamp_answers_what_has_happened_to_its_entities_since() {
        let at = Generation::synthetic("v00000", 1, 0, Overlay::new(), IngestBuffer::new());
        let stamp = Stamp::of(&at);
        let later = |edits: u64, folds: u64, frees: u64| {
            at.with(|g| {
                g.edit_epoch += edits;
                g.fold_epoch += folds;
                g.free_epoch += frees;
            })
        };
        assert_eq!(stamp.since(&at), Since::Still, "nothing since");
        assert_eq!(
            stamp.since(&later(0, 1, 0)),
            Since::Still,
            "a fold freeing nothing"
        );
        assert_eq!(stamp.since(&later(1, 0, 0)), Since::Edited, "an edit");
        assert_eq!(stamp.since(&later(2, 0, 0)), Since::Edited, "two edits");
        assert_eq!(
            stamp.since(&later(1, 1, 0)),
            Since::Folded,
            "an edit and a fold"
        );
        assert_eq!(
            stamp.since(&later(0, 1, 1)),
            Since::Folded,
            "a fold freeing ids"
        );
        assert_eq!(
            stamp.since(&later(1, 1, 1)),
            Since::Folded,
            "an edit and a fold freeing ids"
        );
    }

    #[test]
    fn an_identifier_is_formed_at_the_tenancy_the_index_holds() {
        let dir = tempfile::tempdir().unwrap();
        let generation = generation_with(dir.path(), &[(7, 3)]);
        let key = IdentityKey::from_hex(KEY).unwrap();
        let shard = generation.only_shard();
        let numbers = numbers_of(shard, &[EntityId::new(7), EntityId::new(8)]).unwrap();
        let ids = ids_of_numbers(&key, shard, &numbers).unwrap();
        assert_eq!(ids, [id(&key, 0, 3, 7), id(&key, 0, 0, 8)]);
        assert!(ids
            .iter()
            .all(|id| number_named(&key, &generation, *id).is_some()));
    }
}
