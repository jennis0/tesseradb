//! A point shard's number, a layer space's number, a row and an item each named together with its
//! shard, and what a resolved `mosaica_id` names.

use crate::{EntityId, IdentityError, RowId};

/// A point shard's number, below [`ShardId::LIMIT`].
#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Debug)]
#[cfg_attr(
    feature = "serde",
    derive(serde::Serialize, serde::Deserialize),
    serde(try_from = "u32")
)]
pub struct ShardId(u32);

impl ShardId {
    /// The bits an item's identifier gives its shard.
    pub const BITS: u32 = 19;
    /// The lowest shard id that does not fit.
    pub const LIMIT: u32 = 1 << Self::BITS;
    pub const ZERO: ShardId = ShardId(0);

    /// Refuses a shard id of [`ShardId::LIMIT`] or more.
    pub fn new(raw: u32) -> Result<ShardId, IdentityError> {
        if raw >= Self::LIMIT {
            return Err(IdentityError::ShardOutOfRange { shard: raw });
        }
        Ok(ShardId(raw))
    }

    #[inline]
    pub const fn raw(self) -> u32 {
        self.0
    }
}

impl std::fmt::Display for ShardId {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}", self.0)
    }
}

impl TryFrom<u32> for ShardId {
    type Error = IdentityError;

    fn try_from(raw: u32) -> Result<ShardId, IdentityError> {
        ShardId::new(raw)
    }
}

/// A layer space's number, below [`LayerSpace::LIMIT`]. Each registered layer has one, and no
/// number is given to a second layer.
#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Debug)]
#[cfg_attr(
    feature = "serde",
    derive(serde::Serialize, serde::Deserialize),
    serde(try_from = "u32")
)]
pub struct LayerSpace(u32);

impl LayerSpace {
    /// The bits an artifact's identifier gives its layer space.
    pub const BITS: u32 = 31;
    /// The lowest layer space that does not fit.
    pub const LIMIT: u32 = 1 << Self::BITS;
    pub const ZERO: LayerSpace = LayerSpace(0);

    /// Refuses a layer space of [`LayerSpace::LIMIT`] or more.
    pub fn new(raw: u32) -> Result<LayerSpace, IdentityError> {
        if raw >= Self::LIMIT {
            return Err(IdentityError::LayerSpaceOutOfRange { space: raw });
        }
        Ok(LayerSpace(raw))
    }

    #[inline]
    pub const fn raw(self) -> u32 {
        self.0
    }
}

impl std::fmt::Display for LayerSpace {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}", self.0)
    }
}

impl TryFrom<u32> for LayerSpace {
    type Error = IdentityError;

    fn try_from(raw: u32) -> Result<LayerSpace, IdentityError> {
        LayerSpace::new(raw)
    }
}

/// Whether a point shard takes new items at its high water. A sealed shard takes new items only
/// into ids its compactions freed, and edits of its own items; nothing reopens it.
#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Debug)]
#[cfg_attr(
    feature = "serde",
    derive(serde::Serialize, serde::Deserialize),
    serde(rename_all = "lowercase")
)]
pub enum ShardState {
    Open,
    Sealed,
}

/// What a resolved `mosaica_id` names: an item's entity in its shard, or an artifact's or a layer's
/// entity in its layer space.
#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Debug)]
#[cfg_attr(feature = "serde", derive(serde::Serialize, serde::Deserialize))]
pub enum Target {
    Item(ShardId, EntityId),
    Artifact(LayerSpace, EntityId),
}

impl Target {
    /// The entity, in the shard or the space the target names.
    pub fn entity(self) -> EntityId {
        match self {
            Target::Item(_, entity) | Target::Artifact(_, entity) => entity,
        }
    }

    /// The item's entity, or `None` for an artifact or a layer.
    pub fn item(self) -> Option<EntityId> {
        match self {
            Target::Item(_, entity) => Some(entity),
            Target::Artifact(..) => None,
        }
    }
}

/// An item's entity in its shard: what resolution names and a write addresses. The same entity
/// number in two shards is two items. Ordered shard-major.
#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Debug)]
pub struct ItemAt {
    pub shard: ShardId,
    pub entity: EntityId,
}

impl ItemAt {
    #[inline]
    pub const fn new(shard: ShardId, entity: EntityId) -> ItemAt {
        ItemAt { shard, entity }
    }

    /// The item as a change names it.
    #[inline]
    pub const fn target(self) -> Target {
        Target::Item(self.shard, self.entity)
    }
}

/// `pairs` grouped by shard, ascending, each group in the order given.
pub fn by_shard<T>(
    pairs: impl IntoIterator<Item = (ShardId, T)>,
) -> std::collections::BTreeMap<ShardId, Vec<T>> {
    let mut groups = std::collections::BTreeMap::<ShardId, Vec<T>>::new();
    for (shard, value) in pairs {
        groups.entry(shard).or_default().push(value);
    }
    groups
}

/// The one item of `items`, for a site that serves one shard of one partition. Panics on any other count.
pub fn only<I: IntoIterator>(items: I) -> I::Item {
    let mut items = items.into_iter();
    match (items.next(), items.next()) {
        (Some(item), None) => item,
        (None, _) => panic!("expected exactly one, found none"),
        (Some(_), Some(_)) => panic!("expected exactly one, found {}", 2 + items.count()),
    }
}

/// The high water at which a point shard seals unless a declaration says otherwise: 2³² − 2²⁸, which
/// leaves a sealed shard 2²⁸ ids for edits of its own items.
pub const SHARD_SIZE_DEFAULT: u64 = (1 << 32) - (1 << 28);

/// Why a shard size cannot be declared.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum ShardSizeError {
    Zero,
}

impl std::fmt::Display for ShardSizeError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            ShardSizeError::Zero => write!(
                f,
                "a shard size of 0 issues no id. Declare a size of 1 or more"
            ),
        }
    }
}

impl std::error::Error for ShardSizeError {}

/// Checks `size` as a shard size. A size at or below an open shard's high water seals it
/// ([`reached_size`]).
pub fn check_shard_size(size: u64) -> Result<(), ShardSizeError> {
    if size == 0 {
        return Err(ShardSizeError::Zero);
    }
    Ok(())
}

/// Whether a shard whose high water is `high_water` has reached `size`, where an open shard
/// seals. A size past [`ENTITY_END`] is reached at the end of the ids.
pub fn reached_size(high_water: u64, size: u64) -> bool {
    high_water >= size.min(ENTITY_END)
}

/// Where entity ids end, exclusive: `u32::MAX` is never issued, so a half-open range's end fits a
/// `u32`.
pub const ENTITY_END: u64 = u32::MAX as u64;

/// Ids a batch needs from a shard's high water, past [`ENTITY_END`].
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct ShardFull {
    pub high_water: u64,
    /// How many ids the batch takes from the high water: one for each new item and each edit that
    /// the shard's freed ids do not cover.
    pub needed: u64,
}

impl std::fmt::Display for ShardFull {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let ShardFull { high_water, needed } = self;
        write!(
            f,
            "this batch needs {needed} ids from the shard's high water {high_water}, past \
             {ENTITY_END}, where the shard's ids end. Send fewer edits of this shard's items"
        )
    }
}

impl std::error::Error for ShardFull {}

/// Checks that a shard whose high water is `high_water` can issue `needed` more from it without
/// passing [`ENTITY_END`]. A batch needing none always fits.
pub fn check_room(high_water: u64, needed: u64) -> Result<(), ShardFull> {
    if needed == 0 {
        return Ok(());
    }
    match high_water.checked_add(needed) {
        Some(reached) if reached <= ENTITY_END => Ok(()),
        _ => Err(ShardFull { high_water, needed }),
    }
}

/// The fewest freed ids a shard must hold before a window gives new items its freed ids ahead of
/// the open shard: 2⁻¹⁰ of the shard size, and at least one.
pub fn reuse_min_free(size: u64) -> u64 {
    (size >> 10).max(1)
}

/// New items a window would place in a shard numbered [`ShardId::LIMIT`] or more, which no
/// identifier can name.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct ShardsExhausted {
    /// How many new items found no shard.
    pub needed: u64,
}

impl std::fmt::Display for ShardsExhausted {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "this batch's last {} new items need a point shard past the {} a bundle holds. \
             Delete and compact items to free ids, or send fewer new items",
            self.needed,
            ShardId::LIMIT
        )
    }
}

impl std::error::Error for ShardsExhausted {}

/// A row in a named shard, ordered shard-major.
#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Debug)]
pub struct ShardRow(u64);

impl ShardRow {
    #[inline]
    pub const fn new(shard: ShardId, row: RowId) -> ShardRow {
        ShardRow(((shard.0 as u64) << 32) | row.raw() as u64)
    }

    #[inline]
    pub const fn shard(self) -> ShardId {
        ShardId((self.0 >> 32) as u32)
    }

    /// The row, or `None` when it is in a shard other than `shard`.
    #[inline]
    pub fn row_in(self, shard: ShardId) -> Option<RowId> {
        (self.shard() == shard).then_some(RowId::new(self.0 as u32))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_shard_id_must_fit_its_bits() {
        let last = ShardId::LIMIT - 1;
        assert_eq!(ShardId::new(last).map(ShardId::raw), Ok(last));
        assert_eq!(
            ShardId::new(ShardId::LIMIT),
            Err(IdentityError::ShardOutOfRange {
                shard: ShardId::LIMIT
            })
        );
    }

    #[cfg(feature = "serde")]
    #[test]
    fn a_shard_id_that_does_not_fit_is_refused_when_read() {
        let last = ShardId::LIMIT - 1;
        let written = serde_json::to_string(&ShardId::new(last).unwrap()).unwrap();
        assert_eq!(written, last.to_string());
        assert_eq!(
            serde_json::from_str::<ShardId>(&written).unwrap().raw(),
            last
        );
        assert!(serde_json::from_str::<ShardId>(&ShardId::LIMIT.to_string()).is_err());
    }

    #[test]
    fn shard_rows_sort_shard_major() {
        let one = ShardId::new(1).unwrap();
        let mut rows = vec![
            ShardRow::new(one, RowId::new(0)),
            ShardRow::new(ShardId::ZERO, RowId::new(u32::MAX)),
            ShardRow::new(ShardId::ZERO, RowId::new(3)),
        ];
        rows.sort();
        assert_eq!(
            rows,
            vec![
                ShardRow::new(ShardId::ZERO, RowId::new(3)),
                ShardRow::new(ShardId::ZERO, RowId::new(u32::MAX)),
                ShardRow::new(one, RowId::new(0)),
            ]
        );
    }

    #[test]
    fn a_shard_size_is_positive() {
        assert_eq!(check_shard_size(0), Err(ShardSizeError::Zero));
        assert_eq!(check_shard_size(1), Ok(()));
        assert_eq!(check_shard_size(u64::MAX), Ok(()));
    }

    #[test]
    fn a_shard_reaches_its_size_at_it_or_at_the_end_of_the_ids() {
        assert!(!reached_size(39, 40));
        assert!(reached_size(40, 40), "at the size");
        assert!(reached_size(41, 40), "past it, as a lowered size leaves it");
        assert!(!reached_size(0, 1));
        assert!(!reached_size(ENTITY_END - 1, u64::MAX));
        assert!(reached_size(ENTITY_END, u64::MAX), "the end of the ids");
    }

    #[test]
    fn a_batch_fits_up_to_the_end_of_the_ids_and_no_further() {
        let near = ENTITY_END - 3;
        assert_eq!(check_room(near, 3), Ok(()), "up to the end");
        assert_eq!(
            check_room(near, 4),
            Err(ShardFull {
                high_water: near,
                needed: 4
            })
        );
        assert_eq!(
            check_room(ENTITY_END, 0),
            Ok(()),
            "a batch needing no id fits a full shard"
        );
        assert_eq!(
            check_room(u64::MAX - 1, 2),
            Err(ShardFull {
                high_water: u64::MAX - 1,
                needed: 2
            }),
            "a count past every integer is refused, not wrapped"
        );
    }

    #[test]
    fn the_reuse_threshold_is_a_fraction_of_the_size_and_never_zero() {
        assert_eq!(reuse_min_free(131_073), 128);
        assert_eq!(reuse_min_free(SHARD_SIZE_DEFAULT), 3_932_160);
        assert_eq!(reuse_min_free(1_023), 1);
        assert_eq!(reuse_min_free(1), 1);
    }

    #[test]
    fn a_row_is_given_back_only_in_its_own_shard() {
        let last = ShardId::new(ShardId::LIMIT - 1).unwrap();
        let row = ShardRow::new(last, RowId::new(u32::MAX));
        assert_eq!(row.shard(), last);
        assert_eq!(row.row_in(last), Some(RowId::new(u32::MAX)));
        assert_eq!(row.row_in(ShardId::ZERO), None);
    }
}
