//! What an artifact's membership is, and where the canonical copy lives.
//!
//! **Entity space, always, for the durable form.** Entity ids are permanent and slice-invariant;
//! row space is per slice, derived, and renumbered globally by every fold. A membership stored in
//! row space would be a frozen projection — correct until the first fold, then silently naming
//! other people's documents. The row form is built from this one at open and rebuilt when the
//! generation moves (`annotation-representation.md` §2.1), and it never travels.
//!
//! **The row form is built member-wise and never range-wise**, which is a disclosure rule rather
//! than an implementation note. Translating an entity *range* to a row range would let a Morton
//! neighbour — a document that happens to sit next to a member in row order and belongs to nobody's
//! membership — join the set, and a single extra member can lift an artifact over its existence
//! criterion. The write cycle already forbids range-wise translation for exactly this reason; the
//! same rule reaches the build of the resident form.
//!
//! ## What is not stored, and why the absence is the design
//!
//! **No bounding box.** An earlier draft kept a build-time box over full membership and served an
//! artifact wherever that box intersected the viewport — which discloses the unmasked extent by
//! panning: a viewer sees the edge of a shape in a region holding nothing they may see. Candidacy is
//! a masked question instead, answered from the row form against the viewer's own mask, so no
//! representation here can express the fault.
//!
//! **No unmasked count on any wire shape.** [`ArtifactRecord::declared_size`] is the artifact's
//! full membership size and it exists for exactly one consumer: the proportional existence
//! criterion, which divides by it. It is a **predicate input** — the build computes it, the test
//! consumes it, and nothing serialises it to a client — because a corpus-wide count over items a
//! principal may not see is C8's row, one careless line from being served beside a masked one.

use std::collections::BTreeMap;

use croaring::{Bitmap, Portable};
use tessera_types::EntityId;

/// One artifact's durable state, as the registry holds it.
#[derive(Debug, Clone)]
pub struct ArtifactRecord {
    /// This artifact's own entity — its address for the deny lane, and what `tessera_id` blinds.
    pub entity: EntityId,
    /// The caller's own key, if they supplied one. **Effectively mandatory for a layer another
    /// layer's edges point into**: an edge names its target, and at publish time the caller holds
    /// no `tessera_id` for it.
    pub stable_key: Option<String>,
    /// Entity-space membership — the canonical, slice-invariant record.
    pub members: Bitmap,
}

impl ArtifactRecord {
    /// The artifact's **declared** membership size: how many members it was published with,
    /// unmasked.
    ///
    /// **A predicate input and never a field.** The proportional criterion divides by it; nothing
    /// else may read it, and nothing serialises it. Deriving it from `members` rather than storing
    /// it separately is deliberate — a stored copy is a number someone can reach for, and this one
    /// cannot drift from the set it describes.
    pub fn declared_size(&self) -> u64 {
        self.members.cardinality()
    }
}

/// Every artifact of every layer, keyed by `(layer, level, ordinal)`.
///
/// **Ordinals are dense within a level**, so a level is a vector rather than a map: the address is
/// `entity − run.start` arithmetic, and an ordinal that names no artifact is a hole rather than a
/// lookup miss. A hole is a real state — an artifact whose publication is still in flight, or one
/// a fold has yet to remove — and it must answer *absent* rather than panic.
#[derive(Debug, Clone, Default)]
pub struct ArtifactStore {
    levels: BTreeMap<(String, u32), Vec<Option<ArtifactRecord>>>,
}

impl ArtifactStore {
    pub fn new() -> Self {
        Self::default()
    }

    /// Insert or replace one artifact. Growing the level's vector to fit is what makes a
    /// publication that arrives out of ordinal order land correctly.
    pub fn put(&mut self, layer: &str, level: u32, ordinal: u32, record: ArtifactRecord) {
        let slots = self
            .levels
            .entry((layer.to_string(), level))
            .or_default();
        let idx = ordinal as usize;
        if slots.len() <= idx {
            slots.resize(idx + 1, None);
        }
        slots[idx] = Some(record);
    }

    pub fn get(&self, layer: &str, level: u32, ordinal: u32) -> Option<&ArtifactRecord> {
        self.levels
            .get(&(layer.to_string(), level))
            .and_then(|slots| slots.get(ordinal as usize))
            .and_then(Option::as_ref)
    }

    /// Every artifact of one level, with its ordinal. Holes are skipped.
    pub fn level(&self, layer: &str, level: u32) -> impl Iterator<Item = (u32, &ArtifactRecord)> {
        self.levels
            .get(&(layer.to_string(), level))
            .into_iter()
            .flat_map(|slots| {
                slots
                    .iter()
                    .enumerate()
                    .filter_map(|(i, slot)| slot.as_ref().map(|r| (i as u32, r)))
            })
    }

    /// Every artifact of every level of one layer, as `(level, ordinal, record)`.
    pub fn layer<'a>(
        &'a self,
        layer: &'a str,
    ) -> impl Iterator<Item = (u32, u32, &'a ArtifactRecord)> + 'a {
        self.levels
            .range((layer.to_string(), 0)..)
            .take_while(move |((l, _), _)| l == layer)
            .flat_map(|((_, level), slots)| {
                slots
                    .iter()
                    .enumerate()
                    .filter_map(move |(i, slot)| slot.as_ref().map(|r| (*level, i as u32, r)))
            })
    }

    /// Drop every artifact of a layer — what a layer drop leaves behind otherwise.
    pub fn remove_layer(&mut self, layer: &str) {
        self.levels.retain(|(l, _), _| l != layer);
    }

    /// How many artifacts are held, across every layer. **Operator-facing only**: a per-layer count
    /// is a corpus-wide count over objects a principal may not individually see, which is C8's row,
    /// and this deliberately offers no way to ask for one.
    pub fn total(&self) -> usize {
        self.levels
            .values()
            .map(|slots| slots.iter().filter(|s| s.is_some()).count())
            .sum()
    }

    pub fn is_empty(&self) -> bool {
        self.total() == 0
    }
}

/// Serialise a membership for the WAL, in CRoaring's portable form.
///
/// Portable rather than the frozen form the fragment cache uses: frozen is an mmap-oriented layout
/// with alignment padding and no cross-version guarantee, and this goes into a log that must be
/// readable by the process that reopens it. The bytes are self-describing enough that a corrupt
/// record fails to deserialise rather than yielding a plausible wrong set.
pub fn serialise_members(members: &Bitmap) -> Vec<u8> {
    members.serialize::<Portable>()
}

/// The inverse, refusing bytes that are not a bitmap.
///
/// **A refusal, not a default.** An empty membership is a real and meaningful state — an artifact
/// every one of whose members has been deleted — so decoding damage to "empty" would make a
/// corrupted record indistinguishable from a legitimately emptied artifact, and the second is
/// served rather than refused.
pub fn deserialise_members(bytes: &[u8]) -> Option<Bitmap> {
    Bitmap::try_deserialize::<Portable>(bytes)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn record(entity: u64, members: &[u32]) -> ArtifactRecord {
        ArtifactRecord {
            entity: EntityId::new(entity),
            stable_key: None,
            members: Bitmap::of(members),
        }
    }

    #[test]
    fn a_level_is_dense_and_a_hole_answers_absent() {
        // A publication that skips an ordinal — one artifact still in flight, or one a fold has
        // removed — must leave a hole that answers `None`, not a panic and not a neighbour.
        let mut store = ArtifactStore::new();
        store.put("clusters/a", 0, 5, record(100, &[1, 2, 3]));
        assert!(store.get("clusters/a", 0, 5).is_some());
        assert!(store.get("clusters/a", 0, 0).is_none());
        assert!(store.get("clusters/a", 0, 9).is_none());
        assert_eq!(store.level("clusters/a", 0).count(), 1);
        assert_eq!(store.total(), 1);
    }

    #[test]
    fn the_declared_size_is_derived_and_cannot_drift() {
        // It is the proportional criterion's denominator and nothing else. Deriving it means a
        // deletion that shrinks the membership shrinks the denominator with it, in one place.
        let mut r = record(100, &[1, 2, 3, 4]);
        assert_eq!(r.declared_size(), 4);
        r.members.remove(3);
        assert_eq!(r.declared_size(), 3);
    }

    #[test]
    fn dropping_a_layer_takes_its_artifacts_with_it() {
        let mut store = ArtifactStore::new();
        store.put("clusters/a", 0, 0, record(100, &[1]));
        store.put("clusters/a", 1, 0, record(101, &[2]));
        store.put("clusters/b", 0, 0, record(102, &[3]));
        assert_eq!(store.total(), 3);

        store.remove_layer("clusters/a");
        assert_eq!(store.total(), 1);
        assert!(store.get("clusters/b", 0, 0).is_some());
        assert_eq!(store.layer("clusters/a").count(), 0);
    }

    #[test]
    fn a_membership_round_trips_and_damage_is_refused_rather_than_emptied() {
        let members = Bitmap::of(&[1, 2, 3, 70_000, 4_000_000]);
        let bytes = serialise_members(&members);
        assert_eq!(deserialise_members(&bytes), Some(members));

        // An empty membership is a real state — every member deleted — so damage must not decode
        // to it. That would make a corrupt record indistinguishable from a legitimately emptied
        // artifact, and the second is served.
        let empty = Bitmap::new();
        assert_eq!(deserialise_members(&serialise_members(&empty)), Some(empty));
        assert_eq!(deserialise_members(&[0xff, 0xff, 0xff, 0xff]), None);
        assert_eq!(deserialise_members(&[]), None);
    }

    #[test]
    fn layers_do_not_bleed_into_each_other_in_key_order() {
        // The `layer` iterator walks a range of a BTreeMap keyed by `(name, level)`, so a
        // neighbouring name that sorts adjacently must not be picked up.
        let mut store = ArtifactStore::new();
        store.put("clusters/a", 0, 0, record(100, &[1]));
        store.put("clusters/a-suffix", 0, 0, record(101, &[2]));
        store.put("clusters/b", 0, 0, record(102, &[3]));
        assert_eq!(store.layer("clusters/a").count(), 1);
        assert_eq!(store.layer("clusters/a-suffix").count(), 1);
    }
}
