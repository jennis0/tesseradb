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

/// One artifact as a caller offers it, before the engine has given it an ordinal or an entity.
///
/// **Members are entities, resolved at admission.** A caller names them by `tessera_id` and the
/// control plane inverts them once, at the boundary, exactly as `/control/changes` does — so no
/// blinded identifier reaches durable state, where a key rotation would silently redirect it (I10).
#[derive(Debug, Clone, PartialEq)]
pub struct IncomingArtifact {
    pub stable_key: Option<String>,
    pub members: Bitmap,
}

impl IncomingArtifact {
    /// Builds one from resolved entities.
    ///
    /// **The constructor exists so the bitmap type stays inside this crate.** `tessera-server`
    /// assembles these from a resolved batch and carries no Roaring dependency — a layering
    /// `check-layers.sh` holds, and one worth holding: the request plane should be able to name a
    /// membership without being able to do arithmetic on one.
    pub fn from_entities(
        stable_key: Option<String>,
        members: impl IntoIterator<Item = EntityId>,
    ) -> Self {
        let mut bitmap = Bitmap::new();
        for entity in members {
            // Entity space is `u32` by I9, so the narrowing is total.
            bitmap.add(entity.raw() as u32);
        }
        IncomingArtifact {
            stable_key,
            members: bitmap,
        }
    }
}

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
    /// `(layer, level, stable_key) → ordinal`. **An index, not a second copy of the truth**: it
    /// exists so a batch of ten thousand artifacts can be checked for duplicate keys in
    /// `O(n log n)` rather than rescanning the level per artifact, which is `O(n²)` and reachable
    /// at the sizes this stage publishes.
    keys: BTreeMap<(String, u32, String), u32>,
    /// Where the oldest surviving publication sits in the log — the bound rotation may not reclaim
    /// past. See [`ArtifactStore::oldest_wal_pos`].
    oldest_wal_pos: Option<u64>,
}

impl ArtifactStore {
    pub fn new() -> Self {
        Self::default()
    }

    /// Insert or replace one artifact. Growing the level's vector to fit is what makes a
    /// publication that arrives out of ordinal order land correctly.
    pub fn put(&mut self, layer: &str, level: u32, ordinal: u32, record: ArtifactRecord) {
        if let Some(key) = &record.stable_key {
            self.keys
                .insert((layer.to_string(), level, key.clone()), ordinal);
        }
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

    /// The next ordinal a publication into this level would claim.
    ///
    /// **Derived from the level's extent, never stored**, so replay reconstructs it exactly rather
    /// than needing a durable cursor. A hole left by a removal is *not* reused: the entity behind
    /// it is not reclaimed either (decision 0072 is settled and unbuilt), and handing the ordinal
    /// back while the entity stays spent is how the two would come to disagree.
    pub fn next_ordinal(&self, layer: &str, level: u32) -> u32 {
        self.levels
            .get(&(layer.to_string(), level))
            .map(|slots| slots.len() as u32)
            .unwrap_or(0)
    }

    /// The ordinal a caller's own key names in this level, if any.
    pub fn ordinal_of_key(&self, layer: &str, level: u32, key: &str) -> Option<u32> {
        self.keys
            .get(&(layer.to_string(), level, key.to_string()))
            .copied()
    }

    /// The log position of the oldest surviving publication, or `None` if none survives.
    ///
    /// **Rotation may not reclaim past this, and today that pins the log from the first
    /// publication onwards.** Nothing but the WAL carries a membership: a manifest carries the
    /// registry, segments carry rows and postings, and neither carries a Roaring bitmap of who
    /// belongs to a cluster. So reclaiming a member holding an `ArtifactPublish` destroys the only
    /// copy — a served cluster that comes back from a restart with no members, which the existence
    /// criterion then renders as *absent* rather than as an error.
    ///
    /// ⊘ **This is an open question answered fail-closed, not a design.** Where membership lives on
    /// disk is the owner's decision and the one layout question decisions 0074–0081 left open;
    /// until it lands, an unbounded log is the safe direction and a visible one. It is the same
    /// posture `oldest_wal_pos`'s unknown-position arm takes in the ingest buffer, and for the same
    /// reason: a sequence that grows is noticed, a record that vanishes is not.
    pub fn oldest_wal_pos(&self) -> Option<u64> {
        self.oldest_wal_pos
    }

    /// Applies a durable publication — the one path by which memberships enter, taken by both the
    /// live write path and replay.
    ///
    /// `position` is where the record sits in the log. **Replay applies the recorded ordinals and
    /// entities rather than re-deriving them**, on [`crate::LayerRegistry::apply`]'s contract: a
    /// re-derived ordinal would move an artifact under every suppression naming it.
    ///
    /// Records other than a publication are ignored, so a caller can hand the whole replay stream
    /// to this and to the registry alike.
    ///
    /// Returns how many memberships **did not decode** — always zero in any healthy log. The count
    /// is returned rather than logged because this crate carries no tracing dependency by design
    /// (see `check-layers.sh`), and a silent skip is the one outcome this must not have: an
    /// artifact whose members were lost is served as absent, which is indistinguishable from one
    /// that never cleared its criterion.
    #[must_use]
    pub fn apply(&mut self, record: &crate::wal::WalRecord, position: u64) -> usize {
        let crate::wal::WalRecord::ArtifactPublish {
            layer,
            level,
            artifacts,
            ..
        } = record
        else {
            return 0;
        };
        let mut refused = 0;
        for published in artifacts {
            // Damage is a refusal, not an empty membership — see `deserialise_members`. Skipping
            // leaves a hole, which answers *absent*; the alternative decodes a corrupt record to a
            // legitimately emptied artifact and serves it.
            let Some(members) = deserialise_members(&published.members) else {
                refused += 1;
                continue;
            };
            self.put(
                layer,
                *level,
                published.ordinal,
                ArtifactRecord {
                    entity: published.entity,
                    stable_key: published.stable_key.clone(),
                    members,
                },
            );
        }
        self.oldest_wal_pos = Some(match self.oldest_wal_pos {
            Some(existing) => existing.min(position),
            None => position,
        });
        refused
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
    ///
    /// **The log pin is not lowered with them.** A rotation that reclaimed back to where this
    /// layer's publications sat would also reclaim every *other* layer's records in between, and
    /// the pin is a single bound rather than a set. Holding it costs a longer log; recomputing it
    /// wrongly costs a membership.
    pub fn remove_layer(&mut self, layer: &str) {
        self.levels.retain(|(l, _), _| l != layer);
        self.keys.retain(|(l, _, _), _| l != layer);
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
