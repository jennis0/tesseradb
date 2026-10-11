//! A point shard's number, and a row named together with its shard.

use crate::{IdentityError, RowId};

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

impl TryFrom<u32> for ShardId {
    type Error = IdentityError;

    fn try_from(raw: u32) -> Result<ShardId, IdentityError> {
        ShardId::new(raw)
    }
}

/// The high water at which a point shard seals unless a declaration says otherwise: 2³² − 2²⁸, which
/// leaves a sealed shard 2²⁸ ids for edits of its own items.
pub const SHARD_SIZE_DEFAULT: u64 = (1 << 32) - (1 << 28);

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
    fn a_row_is_given_back_only_in_its_own_shard() {
        let last = ShardId::new(ShardId::LIMIT - 1).unwrap();
        let row = ShardRow::new(last, RowId::new(u32::MAX));
        assert_eq!(row.shard(), last);
        assert_eq!(row.row_in(last), Some(RowId::new(u32::MAX)));
        assert_eq!(row.row_in(ShardId::ZERO), None);
    }
}
