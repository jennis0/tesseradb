//! One point shard's slice of a level, as a form of it walks the level.

use std::sync::Arc;

use mosaica_lifecycle::membership::{ArtifactRecord, ArtifactStore};

use super::ArtifactRecords;

/// One point shard's slice of a level, walked over every artifact of the level: one the slice does
/// not hold walks as [`nothing_held`].
pub(crate) struct LevelSlice<'a> {
    shard: &'a ArtifactStore,
    records: &'a ArtifactStore,
    layer: &'a str,
    level: u32,
    /// Whether `shard` is `records` itself rather than a shard's slice of it.
    whole: bool,
    /// Per number of contents, an artifact of which the shard holds nothing.
    empty: Vec<ArtifactRecord>,
    /// The level's records, which a form of the slice takes rather than builds.
    shared: Option<Arc<ArtifactRecords>>,
}

impl<'a> LevelSlice<'a> {
    /// `shard` is a shard's slice of `records`, or `records` itself.
    pub(crate) fn new(
        shard: &'a ArtifactStore,
        records: &'a ArtifactStore,
        layer: &'a str,
        level: u32,
        shared: Option<Arc<ArtifactRecords>>,
    ) -> Self {
        let whole = std::ptr::eq(shard, records);
        let empty = match whole {
            true => Vec::new(),
            false => {
                let most = records
                    .level(layer, level)
                    .map(|(_, record)| record.contents.len())
                    .max()
                    .unwrap_or(0);
                (0..=most).map(nothing_held).collect()
            }
        };
        LevelSlice {
            shard,
            records,
            layer,
            level,
            whole,
            empty,
            shared,
        }
    }

    /// The shard's slice itself.
    pub(crate) fn shard(&self) -> &'a ArtifactStore {
        self.shard
    }

    /// The level's records, where the caller built them.
    pub(crate) fn shared(&self) -> Option<Arc<ArtifactRecords>> {
        self.shared.clone()
    }

    /// Every artifact of the level as the level's records hold it, key included.
    pub(crate) fn records(&self) -> impl Iterator<Item = (u32, &'a ArtifactRecord)> {
        self.records.level(self.layer, self.level)
    }

    /// Every artifact of the level, as [`ArtifactStore::level`] walks it.
    pub(crate) fn level(&self) -> impl Iterator<Item = (u32, &ArtifactRecord)> {
        self.through_slice(self.records.level(self.layer, self.level))
    }

    /// The artifacts of the level drawn in `view`, as [`ArtifactStore::level_in_view`] walks them.
    pub(crate) fn in_view<'v>(
        &'v self,
        view: &'v str,
    ) -> impl Iterator<Item = (u32, &'v ArtifactRecord)> {
        self.through_slice(self.records.level_in_view(self.layer, self.level, view))
    }

    fn through_slice<'v>(
        &'v self,
        records: impl Iterator<Item = (u32, &'v ArtifactRecord)>,
    ) -> impl Iterator<Item = (u32, &'v ArtifactRecord)> {
        let slots = self.shard.slots(self.layer, self.level);
        records.map(move |(ordinal, record)| match self.whole {
            true => (ordinal, record),
            false => (
                ordinal,
                slots
                    .get(ordinal as usize)
                    .and_then(Option::as_deref)
                    .unwrap_or(&self.empty[record.contents.len()]),
            ),
        })
    }
}

/// A record with `contents` empty generating sets and no member. A slice's form reads nothing else
/// of it.
pub(super) fn nothing_held(contents: usize) -> ArtifactRecord {
    ArtifactRecord {
        entity: mosaica_types::EntityId::new(0),
        key: None,
        view: None,
        incarnation: mosaica_types::view::DECLARED_INCARNATION,
        members: croaring::Bitmap::new().into(),
        declares_members: None,
        contents: (0..contents)
            .map(|_| mosaica_lifecycle::membership::ContentSet {
                values: None,
                digest: [0; 32],
                generated_from: croaring::Bitmap::new(),
                cardinality: 0,
            })
            .collect(),
        withdrawn: Box::default(),
        attached_to: None,
        parents: Vec::new(),
        access: Vec::new(),
    }
}
