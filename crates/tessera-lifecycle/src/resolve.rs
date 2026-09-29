//! Which item each row of a batch names: the one rule the build and the service apply.
//!
//! A row names an item by the values that identify one: its `tessera_id` and each non-null value
//! of a unique field. A row whose values name no item creates one, where its batch creates items.
//! A row whose values name one item addresses it. A row whose values name two items is refused,
//! and so is a `tessera_id` naming no live or suppressed item, since a new item is never given a
//! `tessera_id` its caller chose. Within a batch, rows are decided against what was held before
//! it, so two rows naming one item, or setting one unique value, would each be decided without
//! seeing the other: the first in row order is kept and the later ones are refused.
//!
//! A deleted item names nothing: its values may be given to a new item.
//!
//! The pieces are public so that each path applies them to the batch shape it holds.
//! [`name_row`] decides one row from what its identifiers name, [`collisions`] finds the later
//! rows of every run of equal keys, and [`require_identifier`] says whether a batch that addresses
//! items carries a column to address them by. [`resolve`] composes them over a batch held in
//! memory. The service answers [`Holdings`] from a generation; it applies a batch's accepted rows
//! and lists the refused ones, or refuses the whole batch at its first refused row where the caller
//! asks for a strict batch. The linear build answers it from maps over the files read before, one file at a
//! time. The streaming build applies the same pieces to sorted runs, because a probe per row is
//! the random access it cannot afford at 10⁹ rows.

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
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub enum Identifier {
    TesseraId,
    /// The unique field at this declared position.
    Unique(u16),
}

/// What a batch's rows may do. Each path names the service write its batch stands for.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Batch {
    /// Rows create items and set values: an ingest, a view's points.
    Creates,
    /// Rows address items one each and set values: an attribute file, or one view's rows of a
    /// group-scoped attribute's file.
    Edits,
    /// Rows name items, many rows to an item: a members file, an access relation.
    Names,
}

impl Batch {
    fn creates(self) -> bool {
        self == Batch::Creates
    }

    /// Whether two rows naming one item are one too many.
    fn one_row_per_item(self) -> bool {
        self != Batch::Names
    }

    /// Whether a row's unique values are set on the item it names or creates.
    fn sets_values(self) -> bool {
        matches!(self, Batch::Creates | Batch::Edits)
    }
}

/// Why a row is refused. Each names rows by their position in the batch.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Refusal {
    /// A row's values name more than one item: each identifier with the item it names.
    NamesTwo {
        row: usize,
        named: Vec<(Identifier, EntityId)>,
    },
    /// A row's `tessera_id` names no live or suppressed item.
    UnknownTesseraId { row: usize },
    /// A row addresses an item and names none, in a batch that creates no items.
    NamesNoItem { row: usize },
    /// A later row names the item an earlier row names: `rows` is the kept row and the refused one.
    OneItemTwice { rows: [usize; 2], item: EntityId },
    /// A later row sets a unique value an earlier row sets: `rows` is the kept row and the refused
    /// one.
    OneValueTwice { rows: [usize; 2], field: Identifier },
}

impl Refusal {
    /// The refused row.
    pub fn row(&self) -> usize {
        match self {
            Refusal::NamesTwo { row, .. }
            | Refusal::UnknownTesseraId { row }
            | Refusal::NamesNoItem { row } => *row,
            Refusal::OneItemTwice { rows, .. } | Refusal::OneValueTwice { rows, .. } => rows[1],
        }
    }

    /// The order the rule decides refusals in: a row's own values first, then rows against each
    /// other.
    fn stage(&self) -> u8 {
        match self {
            Refusal::UnknownTesseraId { .. } => 0,
            Refusal::NamesTwo { .. } => 1,
            Refusal::NamesNoItem { .. } => 2,
            Refusal::OneItemTwice { .. } => 3,
            Refusal::OneValueTwice { .. } => 4,
        }
    }

    pub const NAMES_TWO: &'static str = "names_two_items";
    pub const UNKNOWN_TESSERA_ID: &'static str = "unknown_tessera_id";
    pub const NAMES_NO_ITEM: &'static str = "names_no_item";
    pub const ONE_ITEM_TWICE: &'static str = "one_item_twice";
    pub const ONE_VALUE_TWICE: &'static str = "one_value_twice";

    /// Why the row is refused, without the rows and items the refusal names.
    pub fn kind(&self) -> Reason {
        match self {
            Refusal::NamesTwo { .. } => Reason::NamesTwo,
            Refusal::UnknownTesseraId { .. } => Reason::UnknownTesseraId,
            Refusal::NamesNoItem { .. } => Reason::NamesNoItem,
            Refusal::OneItemTwice { .. } => Reason::OneItemTwice,
            Refusal::OneValueTwice { .. } => Reason::OneValueTwice,
        }
    }

    /// The reason as a report spells it.
    pub fn reason(&self) -> &'static str {
        self.kind().as_str()
    }
}

/// Why a row is refused, as a receipt records it.
///
/// On-disk format: variants are positional under postcard, since an ingest record's receipt
/// carries them.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, serde::Serialize, serde::Deserialize)]
pub enum Reason {
    NamesTwo,
    UnknownTesseraId,
    NamesNoItem,
    OneItemTwice,
    OneValueTwice,
}

impl Reason {
    /// The reason as a receipt and a report spell it.
    pub fn as_str(self) -> &'static str {
        match self {
            Reason::NamesTwo => Refusal::NAMES_TWO,
            Reason::UnknownTesseraId => Refusal::UNKNOWN_TESSERA_ID,
            Reason::NamesNoItem => Refusal::NAMES_NO_ITEM,
            Reason::OneItemTwice => Refusal::ONE_ITEM_TWICE,
            Reason::OneValueTwice => Refusal::ONE_VALUE_TWICE,
        }
    }
}

/// What the rule decided for one row.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Verdict {
    Creates,
    Names(EntityId),
    Refused(Refusal),
}

/// What one row's identifiers name, before the rows of its batch are compared.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Named {
    Nothing,
    One(EntityId),
    Two,
}

/// Decide one row from each identifier it carries and the item that identifier names.
pub fn name_row(named: &[(Identifier, EntityId)]) -> Named {
    let Some(&(_, first)) = named.first() else {
        return Named::Nothing;
    };
    if named.iter().any(|&(_, entity)| entity != first) {
        return Named::Two;
    }
    Named::One(first)
}

/// Every later row of a run of equal keys, with the run's first row: `(kept, refused)`. `sorted`
/// ascends by `(key, row)`.
pub fn collisions<K: PartialEq>(
    sorted: impl IntoIterator<Item = (K, usize)>,
) -> impl Iterator<Item = (usize, usize)> {
    let mut first: Option<(K, usize)> = None;
    sorted.into_iter().filter_map(move |(key, row)| match &first {
        Some((held, kept)) if *held == key => Some((*kept, row)),
        _ => {
            first = Some((key, row));
            None
        }
    })
}

/// Why a batch that addresses items cannot be resolved at all.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct NoIdentifier;

impl std::fmt::Display for NoIdentifier {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(
            "it has no column to name items by. Add a `tessera_id` column or a column of a \
             field declared `unique`",
        )
    }
}

/// Whether a batch that addresses existing items carries a column to address them by. A batch
/// that creates items needs none: each row without an identifier is a new item.
pub fn require_identifier(tessera_id_column: bool, unique_columns: usize) -> Result<(), NoIdentifier> {
    match tessera_id_column || unique_columns > 0 {
        true => Ok(()),
        false => Err(NoIdentifier),
    }
}

/// The rule over a batch held in memory: each row's verdict against `holdings`.
pub fn resolve<H: Holdings>(
    rows: &[RowIdentity],
    holdings: &H,
    batch: Batch,
) -> Result<Vec<Verdict>, H::Error> {
    // Each row's identifiers, with the item each names, gathered one lookup per field.
    let mut named: Vec<Vec<(Identifier, EntityId)>> = vec![Vec::new(); rows.len()];
    let mut verdicts: Vec<Option<Verdict>> = vec![None; rows.len()];

    let tessera: Vec<(usize, TesseraId)> = rows
        .iter()
        .enumerate()
        .filter_map(|(at, row)| row.tessera_id.map(|id| (at, id)))
        .collect();
    let ids: Vec<TesseraId> = tessera.iter().map(|(_, id)| *id).collect();
    for ((at, _), holder) in tessera.iter().zip(holdings.tessera_holders(&ids)?) {
        match holder {
            Some(entity) => named[*at].push((Identifier::TesseraId, entity)),
            None => {
                verdicts[*at] = Some(Verdict::Refused(Refusal::UnknownTesseraId { row: *at }))
            }
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

    for (at, named) in named.into_iter().enumerate() {
        if verdicts[at].is_some() {
            continue;
        }
        verdicts[at] = Some(match name_row(&named) {
            Named::Two => Verdict::Refused(Refusal::NamesTwo { row: at, named }),
            Named::One(entity) => Verdict::Names(entity),
            Named::Nothing if batch.creates() => Verdict::Creates,
            Named::Nothing => Verdict::Refused(Refusal::NamesNoItem { row: at }),
        });
    }
    let mut verdicts: Vec<Verdict> = verdicts.into_iter().map(|v| v.expect("decided")).collect();

    if batch.one_row_per_item() {
        let mut items: Vec<(EntityId, usize)> = verdicts
            .iter()
            .enumerate()
            .filter_map(|(at, v)| match v {
                Verdict::Names(entity) => Some((*entity, at)),
                _ => None,
            })
            .collect();
        items.sort_unstable();
        let refused: Vec<(usize, usize)> = collisions(items.iter().copied()).collect();
        for (kept, at) in refused {
            let Verdict::Names(item) = verdicts[at] else {
                unreachable!("only rows naming an item collide on one")
            };
            verdicts[at] = Verdict::Refused(Refusal::OneItemTwice {
                rows: [kept, at],
                item,
            });
        }
    }

    // Two rows setting one value name no one item here, or the rule above would have met them.
    if batch.sets_values() {
        let mut claimed: FxHashMap<(u16, Key), usize> = FxHashMap::default();
        for (at, row) in rows.iter().enumerate() {
            if matches!(verdicts[at], Verdict::Refused(_)) {
                continue;
            }
            verdicts[at] = match one_value_twice(&mut claimed, at, &row.unique) {
                Some(refusal) => Verdict::Refused(refusal),
                None => continue,
            };
        }
    }
    Ok(verdicts)
}

/// The refusal of row `at` if an earlier kept row set one of its `values`, and otherwise nothing,
/// the row's values then being claimed. Rows are offered in row order, each kept row's values
/// claimed before the next is offered, so of the rows setting one value the first kept is the
/// first offered.
pub fn one_value_twice(
    claimed: &mut FxHashMap<(u16, Key), usize>,
    at: usize,
    values: &[(u16, Key)],
) -> Option<Refusal> {
    if let Some(&(field, key)) = values.iter().find(|v| claimed.contains_key(v)) {
        return Some(Refusal::OneValueTwice {
            rows: [claimed[&(field, key)], at],
            field: Identifier::Unique(field),
        });
    }
    for value in values {
        claimed.insert(*value, at);
    }
    None
}

/// The refusal a batch refused whole is refused for: the first the rule decides, and the first row
/// among those.
pub fn first_refusal(verdicts: &[Verdict]) -> Option<&Refusal> {
    verdicts
        .iter()
        .filter_map(|v| match v {
            Verdict::Refused(refusal) => Some(refusal),
            _ => None,
        })
        .min_by_key(|refusal| (refusal.stage(), refusal.row()))
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

    fn resolved(rows: &[RowIdentity], batch: Batch) -> Vec<Verdict> {
        resolve(rows, &held(), batch).unwrap()
    }

    fn refused(refusal: Refusal) -> Verdict {
        Verdict::Refused(refusal)
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
        assert_eq!(
            resolved(&rows, Batch::Creates),
            vec![
                Verdict::Names(e(1)),
                Verdict::Names(e(2)),
                Verdict::Creates,
                Verdict::Creates
            ]
        );
    }

    #[test]
    fn name_row_is_nothing_one_or_two() {
        assert_eq!(name_row(&[]), Named::Nothing);
        let one = [(Identifier::TesseraId, e(3)), (Identifier::Unique(0), e(3))];
        assert_eq!(name_row(&one), Named::One(e(3)));
        let two = [(Identifier::Unique(0), e(3)), (Identifier::Unique(1), e(4))];
        assert_eq!(name_row(&two), Named::Two);
    }

    /// Values naming two items refuse that row and no other, and the refusal says which named
    /// which.
    #[test]
    fn values_naming_two_items_refuse_the_row() {
        let row = RowIdentity {
            tessera_id: Some(TesseraId::new(102)),
            unique: vec![(0, 17)],
        };
        assert_eq!(
            resolved(&[RowIdentity::default(), row], Batch::Creates),
            vec![
                Verdict::Creates,
                refused(Refusal::NamesTwo {
                    row: 1,
                    named: vec![(Identifier::TesseraId, e(2)), (Identifier::Unique(0), e(1))],
                })
            ]
        );
    }

    #[test]
    fn a_tessera_id_naming_nothing_is_refused() {
        let row = RowIdentity {
            tessera_id: Some(TesseraId::new(7)),
            ..RowIdentity::default()
        };
        assert_eq!(
            resolved(&[row], Batch::Creates),
            vec![refused(Refusal::UnknownTesseraId { row: 0 })]
        );
    }

    /// Two rows naming one item keep the first and refuse the later, and so do two new items
    /// given one value. Nulls are not values and never collide.
    #[test]
    fn the_first_of_two_rows_naming_one_item_or_setting_one_value_is_kept() {
        let by_tessera = RowIdentity {
            tessera_id: Some(TesseraId::new(101)),
            ..RowIdentity::default()
        };
        assert_eq!(
            resolved(&[by_unique(1, 5), by_tessera], Batch::Creates),
            vec![
                Verdict::Names(e(1)),
                refused(Refusal::OneItemTwice {
                    rows: [0, 1],
                    item: e(1)
                })
            ]
        );
        assert_eq!(
            resolved(
                &[by_unique(0, 50), by_unique(1, 6), by_unique(0, 50), by_unique(0, 50)],
                Batch::Creates
            ),
            vec![
                Verdict::Creates,
                Verdict::Creates,
                refused(Refusal::OneValueTwice {
                    rows: [0, 2],
                    field: Identifier::Unique(0)
                }),
                refused(Refusal::OneValueTwice {
                    rows: [0, 3],
                    field: Identifier::Unique(0)
                }),
            ]
        );
        assert_eq!(
            resolved(&[RowIdentity::default(), RowIdentity::default()], Batch::Creates),
            vec![Verdict::Creates, Verdict::Creates]
        );
    }

    /// A batch that creates nothing refuses a row naming nothing, and a batch of names takes any
    /// number of rows naming one item.
    #[test]
    fn what_a_batch_may_do_decides_the_rows_it_refuses() {
        let rows = [by_unique(0, 17), by_unique(0, 99), by_unique(0, 17)];
        assert_eq!(
            resolved(&rows, Batch::Edits),
            vec![
                Verdict::Names(e(1)),
                refused(Refusal::NamesNoItem { row: 1 }),
                refused(Refusal::OneItemTwice {
                    rows: [0, 2],
                    item: e(1)
                }),
            ]
        );
        assert_eq!(
            resolved(&rows, Batch::Names),
            vec![
                Verdict::Names(e(1)),
                refused(Refusal::NamesNoItem { row: 1 }),
                Verdict::Names(e(1)),
            ]
        );
    }

    /// Joe's example: items (a=x, b=p) and (a=k, b=y) exist. (a=x, b=y) names both and is
    /// refused; (a=x, b=q) edits the first; (a=z, b=q) would create, and sets the value the edit
    /// sets, so it is the later of two rows setting one value.
    #[test]
    fn a_row_edits_the_item_its_values_name() {
        let mut held = Held::default();
        held.unique.insert((0, 1), vec![e(0)]); // a = x
        held.unique.insert((1, 10), vec![e(0)]); // b = p
        held.unique.insert((0, 2), vec![e(1)]); // a = k
        held.unique.insert((1, 11), vec![e(1)]); // b = y
        let row = |a: Key, b: Key| RowIdentity {
            unique: vec![(0, a), (1, b)],
            ..RowIdentity::default()
        };
        let verdicts = resolve(&[row(1, 11), row(1, 12), row(3, 12)], &held, Batch::Creates).unwrap();
        assert!(matches!(verdicts[0], Verdict::Refused(Refusal::NamesTwo { row: 0, .. })));
        assert_eq!(verdicts[1], Verdict::Names(e(0)));
        assert_eq!(
            verdicts[2],
            refused(Refusal::OneValueTwice {
                rows: [1, 2],
                field: Identifier::Unique(1)
            })
        );
    }

    #[test]
    fn collisions_keep_the_first_row_of_each_run() {
        let sorted = [(1, 0), (1, 4), (1, 9), (2, 3), (3, 1), (3, 2)];
        assert_eq!(
            collisions(sorted).collect::<Vec<_>>(),
            vec![(0, 4), (0, 9), (1, 2)]
        );
    }

    #[test]
    fn an_addressing_batch_needs_a_column_to_name_items_by() {
        assert_eq!(require_identifier(false, 0), Err(NoIdentifier));
        assert_eq!(require_identifier(true, 0), Ok(()));
        assert_eq!(require_identifier(false, 2), Ok(()));
    }

    /// A batch refused whole is refused for the refusal the rule decides first.
    #[test]
    fn the_first_refusal_is_the_first_stage_then_the_first_row() {
        let rows = [
            by_unique(0, 50),
            by_unique(0, 50),
            RowIdentity {
                tessera_id: Some(TesseraId::new(7)),
                ..RowIdentity::default()
            },
        ];
        let verdicts = resolved(&rows, Batch::Creates);
        assert_eq!(
            first_refusal(&verdicts),
            Some(&Refusal::UnknownTesseraId { row: 2 })
        );
    }
}
