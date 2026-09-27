//! Which item each row of an ingest batch names.
//!
//! A row names an item by the values that identify one: its `tessera_id` and each non-null value
//! of a unique field. A row whose values name no item creates one. A row
//! whose values name one item addresses it. A row whose values name two items is refused, and so
//! is a `tessera_id` naming no live or suppressed item, since a new item is never given a
//! `tessera_id` its caller chose. Across a batch, two rows may not name one item, and two rows may
//! not set one unique value, since each would be decided without seeing the other.
//!
//! A deleted item names nothing: its values may be given to a new item.
//!
//! Every lookup goes through [`Holdings`], which the engine answers from a generation and the
//! build will answer from its inputs, so both apply the one rule.

use rustc_hash::FxHashMap;
use tessera_types::{EntityId, TesseraId};

/// A unique field's value as its index keys it, widened to 128 bits. Two values of one field
/// are equal exactly where their keys are.
pub type Key = u128;

/// The values one row identifies an item by.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct RowIdentity {
    pub tessera_id: Option<TesseraId>,
    /// `(declared position, key)` for each non-null value of a unique field the row carries.
    pub unique: Vec<(u16, Key)>,
}

/// Who holds what: every answer counts live and suppressed items and leaves deleted ones out.
pub trait Holdings {
    type Error;

    /// For each key, every item holding it in the unique field at declared position `field`.
    fn holders(&self, field: u16, keys: &[Key]) -> Result<Vec<Vec<EntityId>>, Self::Error>;

    /// For each `tessera_id`, the item it names.
    fn tessera_holders(&self, ids: &[TesseraId]) -> Result<Vec<Option<EntityId>>, Self::Error>;
}

/// What identified an item in a refusal.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Identifier {
    TesseraId,
    /// The unique field at this declared position.
    Unique(u16),
}

/// Why a batch is refused. Each names rows by their position in the batch.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Refusal {
    /// A row's values name more than one item: each identifier with the item it names.
    NamesTwo {
        row: usize,
        named: Vec<(Identifier, EntityId)>,
    },
    /// A row's `tessera_id` names no live or suppressed item.
    UnknownTesseraId { row: usize },
    /// Two rows name one item.
    OneItemTwice { rows: [usize; 2], item: EntityId },
    /// Two rows set one value of one unique field and name no one item.
    OneValueTwice { rows: [usize; 2], field: Identifier },
}

/// The item each row names, `None` for a row that creates one, or the first refusal met.
pub fn resolve<H: Holdings>(
    rows: &[RowIdentity],
    holdings: &H,
) -> Result<Result<Vec<Option<EntityId>>, Refusal>, H::Error> {
    // Each row's identifiers, with the item each names, gathered one lookup per field.
    let mut named: Vec<Vec<(Identifier, EntityId)>> = vec![Vec::new(); rows.len()];

    let tessera: Vec<(usize, TesseraId)> = rows
        .iter()
        .enumerate()
        .filter_map(|(at, row)| row.tessera_id.map(|id| (at, id)))
        .collect();
    let ids: Vec<TesseraId> = tessera.iter().map(|(_, id)| *id).collect();
    for ((at, _), holder) in tessera.iter().zip(holdings.tessera_holders(&ids)?) {
        match holder {
            Some(entity) => named[*at].push((Identifier::TesseraId, entity)),
            None => return Ok(Err(Refusal::UnknownTesseraId { row: *at })),
        }
    }

    let mut by_field: FxHashMap<u16, Vec<(usize, Key)>> = FxHashMap::default();
    for (at, row) in rows.iter().enumerate() {
        for (field, key) in &row.unique {
            by_field.entry(*field).or_default().push((at, *key));
        }
    }
    let mut fields: Vec<u16> = by_field.keys().copied().collect();
    fields.sort_unstable();
    for field in fields {
        let settings = &by_field[&field];
        let keys: Vec<Key> = settings.iter().map(|(_, key)| *key).collect();
        for ((at, _), holders) in settings.iter().zip(holdings.holders(field, &keys)?) {
            for entity in holders {
                named[*at].push((Identifier::Unique(field), entity));
            }
        }
    }

    let mut items: Vec<Option<EntityId>> = Vec::with_capacity(rows.len());
    for (at, named) in named.into_iter().enumerate() {
        let first = named.first().map(|(_, entity)| *entity);
        if named.iter().any(|(_, entity)| Some(*entity) != first) {
            return Ok(Err(Refusal::NamesTwo { row: at, named }));
        }
        items.push(first);
    }

    let mut seen: FxHashMap<EntityId, usize> = FxHashMap::default();
    for (at, item) in items.iter().enumerate() {
        if let Some(item) = item {
            if let Some(first) = seen.insert(*item, at) {
                return Ok(Err(Refusal::OneItemTwice {
                    rows: [first, at],
                    item: *item,
                }));
            }
        }
    }

    // Two rows setting one value name no one item here, or the rule above would have met them.
    let mut set: FxHashMap<(Identifier, Key), usize> = FxHashMap::default();
    for (at, row) in rows.iter().enumerate() {
        for (field, key) in &row.unique {
            let field = Identifier::Unique(*field);
            if let Some(first) = set.insert((field, *key), at) {
                return Ok(Err(Refusal::OneValueTwice {
                    rows: [first, at],
                    field,
                }));
            }
        }
    }
    Ok(Ok(items))
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Holdings over fixed maps: `unique[(field, key)]` and `tessera[id]`.
    #[derive(Default)]
    struct Held {
        unique: FxHashMap<(u16, Key), Vec<EntityId>>,
        tessera: FxHashMap<u64, EntityId>,
    }

    impl Holdings for Held {
        type Error = ();
        fn holders(&self, field: u16, keys: &[Key]) -> Result<Vec<Vec<EntityId>>, ()> {
            Ok(keys
                .iter()
                .map(|key| self.unique.get(&(field, *key)).cloned().unwrap_or_default())
                .collect())
        }
        fn tessera_holders(&self, ids: &[TesseraId]) -> Result<Vec<Option<EntityId>>, ()> {
            Ok(ids.iter().map(|id| self.tessera.get(&id.raw()).copied()).collect())
        }
    }

    fn e(n: u64) -> EntityId {
        EntityId::new(n)
    }

    fn held() -> Held {
        let mut held = Held::default();
        held.unique.insert((0, 17), vec![e(1)]);
        held.unique.insert((0, 18), vec![e(2)]);
        held.unique.insert((1, 5), vec![e(1)]);
        held.tessera.insert(101, e(1));
        held.tessera.insert(102, e(2));
        held
    }

    fn by_unique(field: u16, key: Key) -> RowIdentity {
        RowIdentity {
            unique: vec![(field, key)],
            ..RowIdentity::default()
        }
    }

    fn resolved(rows: &[RowIdentity]) -> Result<Vec<Option<EntityId>>, Refusal> {
        resolve(rows, &held()).unwrap()
    }

    /// Each identifier names the item that holds it, several agreeing identifiers name it once,
    /// and a row whose values nobody holds creates an item.
    #[test]
    fn a_row_names_the_one_item_its_values_name() {
        let agreeing = RowIdentity {
            tessera_id: Some(TesseraId::new(101)),
            unique: vec![(0, 17), (1, 5)],
        };
        let rows = [agreeing, by_unique(0, 18), by_unique(0, 99), RowIdentity::default()];
        assert_eq!(resolved(&rows), Ok(vec![Some(e(1)), Some(e(2)), None, None]));
    }

    /// Values naming two items refuse the row, and the refusal says which named which.
    #[test]
    fn values_naming_two_items_are_refused() {
        let row = RowIdentity {
            tessera_id: Some(TesseraId::new(102)),
            unique: vec![(0, 17)],
        };
        assert_eq!(
            resolved(&[RowIdentity::default(), row]),
            Err(Refusal::NamesTwo {
                row: 1,
                named: vec![(Identifier::TesseraId, e(2)), (Identifier::Unique(0), e(1))],
            })
        );
    }

    #[test]
    fn a_tessera_id_naming_nothing_is_refused() {
        let row = RowIdentity {
            tessera_id: Some(TesseraId::new(7)),
            ..RowIdentity::default()
        };
        assert_eq!(resolved(&[row]), Err(Refusal::UnknownTesseraId { row: 0 }));
    }

    /// Two rows naming one item by different values are refused, and so are two new items given
    /// one value. Nulls are not values and never collide.
    #[test]
    fn two_rows_may_not_name_one_item_or_set_one_value() {
        let by_tessera = RowIdentity {
            tessera_id: Some(TesseraId::new(101)),
            ..RowIdentity::default()
        };
        assert_eq!(
            resolved(&[by_unique(1, 5), by_tessera]),
            Err(Refusal::OneItemTwice {
                rows: [0, 1],
                item: e(1)
            })
        );
        assert_eq!(
            resolved(&[by_unique(0, 50), by_unique(1, 6), by_unique(0, 50)]),
            Err(Refusal::OneValueTwice {
                rows: [0, 2],
                field: Identifier::Unique(0)
            })
        );
        assert_eq!(
            resolved(&[RowIdentity::default(), RowIdentity::default()]),
            Ok(vec![None, None])
        );
    }
}
