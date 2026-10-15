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
    /// Below the shard's high water.
    BelowHighWater {
        size: u64,
        high_water: u64,
    },
}

impl std::fmt::Display for ShardSizeError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            ShardSizeError::Zero => write!(
                f,
                "a shard size of 0 issues no id. Declare a size of 1 or more"
            ),
            ShardSizeError::BelowHighWater { size, high_water } => write!(
                f,
                "a size of {size} is below the shard's high water {high_water}. Declare a size \
                 of {high_water} or more"
            ),
        }
    }
}

impl std::error::Error for ShardSizeError {}

/// Checks `size` as the size of a shard whose high water is `high_water`. A build checks it with
/// a high water of 0.
pub fn check_shard_size(size: u64, high_water: u64) -> Result<(), ShardSizeError> {
    if size == 0 {
        return Err(ShardSizeError::Zero);
    }
    if size < high_water {
        return Err(ShardSizeError::BelowHighWater { size, high_water });
    }
    Ok(())
}

/// Where entity ids end, exclusive: `u32::MAX` is never issued, so a half-open range's end fits a
/// `u32`.
pub const ENTITY_END: u64 = u32::MAX as u64;

/// What a shard's high water may not pass: its size, or the end of its id space where that is
/// lower.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum ShardLimit {
    Size(u64),
    End(u64),
}

/// Ids a batch needs from a shard's high water, past what the shard may issue.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct ShardFull {
    pub limit: ShardLimit,
    pub high_water: u64,
    /// How many ids the batch takes from the high water: one for each new item and each edit that
    /// the shard's freed ids do not cover.
    pub needed: u64,
}

impl std::fmt::Display for ShardFull {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let ShardFull {
            limit,
            high_water,
            needed,
        } = self;
        match limit {
            ShardLimit::Size(size) => write!(
                f,
                "this batch needs {needed} ids from the shard's high water {high_water}, past its \
                 size {size}. Declare a shard size of at least {}, or send fewer new items and \
                 edits",
                high_water.saturating_add(*needed)
            ),
            ShardLimit::End(end) => write!(
                f,
                "this batch needs {needed} ids from the shard's high water {high_water}, past \
                 {end}, where the shard's ids end. Send fewer new items and edits"
            ),
        }
    }
}

impl std::error::Error for ShardFull {}

/// Checks that a shard whose ids end at `end` and whose high water is `high_water` can issue
/// `needed` more from it without passing `size`. A batch needing none always fits. The refusal
/// names the lower of `size` and `end`.
pub fn check_room(size: u64, end: u64, high_water: u64, needed: u64) -> Result<(), ShardFull> {
    if needed == 0 {
        return Ok(());
    }
    let (limit, bound) = if size <= end {
        (ShardLimit::Size(size), size)
    } else {
        (ShardLimit::End(end), end)
    };
    match high_water.checked_add(needed) {
        Some(reached) if reached <= bound => Ok(()),
        _ => Err(ShardFull {
            limit,
            high_water,
            needed,
        }),
    }
}

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
    fn a_shard_size_is_positive_and_not_below_the_high_water() {
        assert_eq!(check_shard_size(0, 0), Err(ShardSizeError::Zero));
        assert_eq!(check_shard_size(1, 0), Ok(()));
        assert_eq!(
            check_shard_size(40, 40),
            Ok(()),
            "a full shard may keep its size"
        );
        assert_eq!(
            check_shard_size(39, 40),
            Err(ShardSizeError::BelowHighWater {
                size: 39,
                high_water: 40
            })
        );
        assert_eq!(check_shard_size(u64::MAX, 40), Ok(()));
    }

    #[test]
    fn a_batch_fits_up_to_the_size_and_no_further() {
        assert_eq!(check_room(10, 100, 7, 3), Ok(()), "up to the size");
        assert_eq!(
            check_room(10, 100, 7, 4),
            Err(ShardFull {
                limit: ShardLimit::Size(10),
                high_water: 7,
                needed: 4
            })
        );
        assert_eq!(
            check_room(10, 100, 10, 0),
            Ok(()),
            "a batch needing no id fits a full shard"
        );
        assert_eq!(check_room(10, 100, 12, 0), Ok(()));
    }

    #[test]
    fn the_lower_of_the_size_and_the_end_of_the_ids_is_the_limit_named() {
        assert_eq!(
            check_room(1_000, 100, 98, 3),
            Err(ShardFull {
                limit: ShardLimit::End(100),
                high_water: 98,
                needed: 3
            })
        );
        assert_eq!(
            check_room(100, 100, 98, 3).map_err(|full| full.limit),
            Err(ShardLimit::Size(100)),
            "a size at the end is the size"
        );
        assert_eq!(
            check_room(u64::MAX, u64::MAX, u64::MAX - 1, 2).map_err(|full| full.limit),
            Err(ShardLimit::Size(u64::MAX)),
            "a count past every integer is refused, not wrapped"
        );
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
