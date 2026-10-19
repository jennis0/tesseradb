//! Which item each row of a batch names: the one rule the build and the service apply.
//!
//! A row names an item by the values that identify one: its `mosaica_id` and each non-null value
//! of a unique field. A row whose values name no item creates one, where its batch creates items.
//! A row whose values name one item addresses it. A row whose values name two items is refused,
//! and so is a `mosaica_id` naming no live or suppressed item, since a new item is never given a
//! `mosaica_id` its caller chose. Within a batch, rows are decided against what was held before
//! it, so two rows naming one item, or setting one unique value, would each be decided without
//! seeing the other: the first in row order is kept and the later ones are refused. Rows are
//! decided in row order against the rows kept before them, so a refused row claims neither its
//! item nor its values and never refuses a later row. Where two reasons apply to one row, the rule
//! gives the first of: a `mosaica_id` naming nothing, values naming two items or a value held
//! outside the writer's terms, naming no item where the batch creates none, an item an earlier row
//! names, a value an earlier row sets.
//!
//! A deleted item names nothing: its values may be given to a new item.
//!
//! A writer whose terms bound its writes names only the items inside them; one outside names
//! nothing, as an absent one does. A row that would create an item, or set a value on the item it
//! names, with a unique value an outside item holds is refused as `value_held`, which says nothing
//! of the holder.
//!
//! The pieces are public so that each path applies them to the batch shape it holds.
//! [`name_row`] decides one row from what its identifiers name, and [`require_identifier`] says
//! whether a batch that addresses items carries a column to address them by. [`resolve`] composes
//! them over a batch held in memory. The service answers [`Holdings`] from a generation; it
//! applies a batch's accepted rows and lists the refused ones, or refuses the whole batch at its
//! first refused row where the caller asks for a strict batch. The linear build answers it from
//! maps over the files read before, one file at a time. The streaming build applies the same
//! decisions to sorted runs and one pass in row order, because a probe per row is the random
//! access it cannot afford at 10⁹ rows.

use rustc_hash::FxHashMap;
use mosaica_types::{ItemAt, MosaicaId};

/// A unique field's value as its index keys it, widened to 128 bits. Two values of one field
/// are equal exactly where their keys are.
pub type Key = u128;

/// The values one row identifies an item by.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct RowIdentity {
    pub mosaica_id: Option<MosaicaId>,
    /// `(declared position, key)` for each non-null value of a unique field the row carries.
    pub unique: Vec<(u16, Key)>,
}

/// Who holds what: every answer counts live and suppressed items and leaves deleted ones out.
pub trait Holdings {
    type Error;

    /// Every item holding a key in the unique field at declared position `field`, as
    /// `(position in keys, holder)`.
    fn holders(&self, field: u16, keys: &[Key]) -> Result<Vec<(usize, Holder)>, Self::Error>;

    /// For each `mosaica_id`, the item it names: `None` for one outside the writer's terms.
    fn mosaica_holders(&self, ids: &[MosaicaId]) -> Result<Vec<Option<ItemAt>>, Self::Error>;
}

/// An item holding a value, as the writer may know it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Holder {
    Inside(ItemAt),
    /// An item outside the writer's terms, of which nothing more is told.
    Outside,
}

/// What identified an item in a refusal.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub enum Identifier {
    MosaicaId,
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
        named: Vec<(Identifier, ItemAt)>,
    },
    /// A row's `mosaica_id` names no live or suppressed item.
    UnknownMosaicaId { row: usize },
    /// A row addresses an item and names none, in a batch that creates no items.
    NamesNoItem { row: usize },
    /// A later row names the item an earlier row names: `rows` is the kept row and the refused one.
    OneItemTwice { rows: [usize; 2], item: ItemAt },
    /// A later row sets a unique value an earlier row sets: `rows` is the kept row and the refused
    /// one.
    OneValueTwice { rows: [usize; 2], field: Identifier },
    /// A row would create an item, where `creates`, or set a value on the item it names, with a
    /// value of `field` that an item outside the writer's terms holds.
    ValueHeld {
        row: usize,
        field: Identifier,
        creates: bool,
    },
    /// A row sets an access label the writer's terms do not satisfy. Decided by the service before
    /// the rule, from the row alone.
    LabelNotHeld { row: usize },
}

impl Refusal {
    /// The refused row.
    pub fn row(&self) -> usize {
        match self {
            Refusal::NamesTwo { row, .. }
            | Refusal::UnknownMosaicaId { row }
            | Refusal::NamesNoItem { row }
            | Refusal::ValueHeld { row, .. }
            | Refusal::LabelNotHeld { row } => *row,
            Refusal::OneItemTwice { rows, .. } | Refusal::OneValueTwice { rows, .. } => rows[1],
        }
    }

    /// The order the rule decides refusals in: a row's own values first, then rows against each
    /// other.
    fn stage(&self) -> u8 {
        match self {
            Refusal::LabelNotHeld { .. } | Refusal::UnknownMosaicaId { .. } => 0,
            Refusal::NamesTwo { .. } | Refusal::ValueHeld { .. } => 1,
            Refusal::NamesNoItem { .. } => 2,
            Refusal::OneItemTwice { .. } => 3,
            Refusal::OneValueTwice { .. } => 4,
        }
    }

    pub const NAMES_TWO: &'static str = "names_two_items";
    pub const UNKNOWN_MOSAICA_ID: &'static str = "unknown_mosaica_id";
    pub const NAMES_NO_ITEM: &'static str = "names_no_item";
    pub const ONE_ITEM_TWICE: &'static str = "one_item_twice";
    pub const ONE_VALUE_TWICE: &'static str = "one_value_twice";
    pub const VALUE_HELD: &'static str = "value_held";
    pub const LABEL_NOT_HELD: &'static str = "label_not_held";

    /// Why the row is refused, without the rows and items the refusal names.
    pub fn kind(&self) -> Reason {
        match self {
            Refusal::NamesTwo { .. } => Reason::NamesTwo,
            Refusal::UnknownMosaicaId { .. } => Reason::UnknownMosaicaId,
            Refusal::NamesNoItem { .. } => Reason::NamesNoItem,
            Refusal::OneItemTwice { .. } => Reason::OneItemTwice,
            Refusal::OneValueTwice { .. } => Reason::OneValueTwice,
            Refusal::ValueHeld { .. } => Reason::ValueHeld,
            Refusal::LabelNotHeld { .. } => Reason::LabelNotHeld,
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
    UnknownMosaicaId,
    NamesNoItem,
    OneItemTwice,
    OneValueTwice,
    ValueHeld,
    LabelNotHeld,
}

impl Reason {
    /// The reason as a receipt and a report spell it.
    pub fn as_str(self) -> &'static str {
        match self {
            Reason::NamesTwo => Refusal::NAMES_TWO,
            Reason::UnknownMosaicaId => Refusal::UNKNOWN_MOSAICA_ID,
            Reason::NamesNoItem => Refusal::NAMES_NO_ITEM,
            Reason::OneItemTwice => Refusal::ONE_ITEM_TWICE,
            Reason::OneValueTwice => Refusal::ONE_VALUE_TWICE,
            Reason::ValueHeld => Refusal::VALUE_HELD,
            Reason::LabelNotHeld => Refusal::LABEL_NOT_HELD,
        }
    }
}

/// What the rule decided for one row.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Verdict {
    Creates,
    Names(ItemAt),
    Refused(Refusal),
}

/// What one row's identifiers name, before the rows of its batch are compared.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Named {
    Nothing,
    One(ItemAt),
    Two,
}

/// Decide one row from each identifier it carries and the item that identifier names.
pub fn name_row(named: &[(Identifier, ItemAt)]) -> Named {
    let Some(&(_, first)) = named.first() else {
        return Named::Nothing;
    };
    if named.iter().any(|&(_, item)| item != first) {
        return Named::Two;
    }
    Named::One(first)
}

/// Why a batch that addresses items cannot be resolved at all.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct NoIdentifier;

impl std::fmt::Display for NoIdentifier {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(
            "it has no column to name items by. Add a `mosaica_id` column or a column of a \
             field declared `unique`",
        )
    }
}

/// Whether a batch that addresses existing items carries a column to address them by. A batch
/// that creates items needs none: each row without an identifier is a new item.
pub fn require_identifier(
    mosaica_id_column: bool,
    unique_columns: usize,
) -> Result<(), NoIdentifier> {
    match mosaica_id_column || unique_columns > 0 {
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
    // Each row's identifiers, with the item each names, gathered one lookup per field into one
    // list, then ordered by row.
    let mut found: Vec<(usize, (Identifier, ItemAt))> = Vec::new();
    let mut verdicts: Vec<Option<Verdict>> = vec![None; rows.len()];

    let mosaica: Vec<(usize, MosaicaId)> = rows
        .iter()
        .enumerate()
        .filter_map(|(at, row)| row.mosaica_id.map(|id| (at, id)))
        .collect();
    let ids: Vec<MosaicaId> = mosaica.iter().map(|(_, id)| *id).collect();
    for ((at, _), holder) in mosaica.iter().zip(holdings.mosaica_holders(&ids)?) {
        match holder {
            Some(item) => found.push((*at, (Identifier::MosaicaId, item))),
            None => verdicts[*at] = Some(Verdict::Refused(Refusal::UnknownMosaicaId { row: *at })),
        }
    }

    // The first value of each row held outside the writer's terms, by row.
    let mut outside: FxHashMap<usize, Identifier> = FxHashMap::default();
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
        for (at, holder) in holdings.holders(field, &keys)? {
            let row = settings[at].0;
            match holder {
                Holder::Inside(item) => found.push((row, (Identifier::Unique(field), item))),
                Holder::Outside => {
                    outside.entry(row).or_insert(Identifier::Unique(field));
                }
            }
        }
    }
    found.sort_by_key(|(at, _)| *at);
    let named: Vec<(Identifier, ItemAt)> = found.iter().map(|(_, named)| *named).collect();

    let mut next = 0;
    for (at, verdict) in verdicts.iter_mut().enumerate() {
        let start = next;
        while next < found.len() && found[next].0 == at {
            next += 1;
        }
        if verdict.is_some() {
            continue;
        }
        let named = &named[start..next];
        // A value held outside is set where the batch sets values, and names nothing otherwise.
        let held = outside.get(&at).copied().filter(|_| batch.sets_values());
        *verdict = Some(match (name_row(named), held) {
            (Named::Two, _) => Verdict::Refused(Refusal::NamesTwo {
                row: at,
                named: named.to_vec(),
            }),
            (Named::One(_), Some(field)) => Verdict::Refused(Refusal::ValueHeld {
                row: at,
                field,
                creates: false,
            }),
            (Named::Nothing, Some(field)) if batch.creates() => {
                Verdict::Refused(Refusal::ValueHeld {
                    row: at,
                    field,
                    creates: true,
                })
            }
            (Named::One(item), None) => Verdict::Names(item),
            (Named::Nothing, None) if batch.creates() => Verdict::Creates,
            (Named::Nothing, _) => Verdict::Refused(Refusal::NamesNoItem { row: at }),
        });
    }
    let mut verdicts: Vec<Verdict> = verdicts.into_iter().map(|v| v.expect("decided")).collect();

    // Rows are decided in row order against the rows kept before them, so a refused row claims
    // neither its item nor its values: of two rows naming one item, or setting one value, the first
    // kept is kept.
    let mut items: FxHashMap<ItemAt, usize> = FxHashMap::default();
    let mut claimed: FxHashMap<(u16, Key), usize> = FxHashMap::default();
    for (at, row) in rows.iter().enumerate() {
        let item = match verdicts[at] {
            Verdict::Refused(_) => continue,
            Verdict::Names(item) => batch.one_row_per_item().then_some(item),
            Verdict::Creates => None,
        };
        if let Some(item) = item {
            if let Some(&kept) = items.get(&item) {
                verdicts[at] = Verdict::Refused(Refusal::OneItemTwice {
                    rows: [kept, at],
                    item,
                });
                continue;
            }
        }
        if batch.sets_values() {
            if let Some(&(field, key)) = row.unique.iter().find(|v| claimed.contains_key(v)) {
                verdicts[at] = Verdict::Refused(Refusal::OneValueTwice {
                    rows: [claimed[&(field, key)], at],
                    field: Identifier::Unique(field),
                });
                continue;
            }
            for value in &row.unique {
                claimed.insert(*value, at);
            }
        }
        if let Some(item) = item {
            items.insert(item, at);
        }
    }
    Ok(verdicts)
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

    /// Holdings over fixed maps: `unique[(field, key)]` and `mosaica[id]`, and the values items
    /// outside the writer's terms hold.
    #[derive(Default)]
    struct Held {
        unique: FxHashMap<(u16, Key), Vec<ItemAt>>,
        mosaica: FxHashMap<u64, ItemAt>,
        outside: Vec<(u16, Key)>,
    }

    impl Holdings for Held {
        type Error = ();
        fn holders(&self, field: u16, keys: &[Key]) -> Result<Vec<(usize, Holder)>, ()> {
            Ok(keys
                .iter()
                .enumerate()
                .flat_map(|(at, key)| {
                    let held = self.unique.get(&(field, *key)).cloned().unwrap_or_default();
                    let outside = self
                        .outside
                        .contains(&(field, *key))
                        .then_some(Holder::Outside);
                    held.into_iter()
                        .map(Holder::Inside)
                        .chain(outside)
                        .map(move |holder| (at, holder))
                })
                .collect())
        }
        fn mosaica_holders(&self, ids: &[MosaicaId]) -> Result<Vec<Option<ItemAt>>, ()> {
            Ok(ids
                .iter()
                .map(|id| self.mosaica.get(&id.raw()).copied())
                .collect())
        }
    }

    /// Entity `n` of shard 0.
    fn e(n: u64) -> ItemAt {
        at(0, n)
    }

    fn at(shard: u32, n: u64) -> ItemAt {
        ItemAt::new(
            mosaica_types::ShardId::new(shard).unwrap(),
            mosaica_types::EntityId::new(n),
        )
    }

    fn held() -> Held {
        let mut held = Held::default();
        held.unique.insert((0, 17), vec![e(1)]);
        held.unique.insert((0, 18), vec![e(2)]);
        held.unique.insert((1, 5), vec![e(1)]);
        held.mosaica.insert(101, e(1));
        held.mosaica.insert(102, e(2));
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
            mosaica_id: Some(MosaicaId::new(101)),
            unique: vec![(0, 17), (1, 5)],
        };
        let rows = [
            agreeing,
            by_unique(0, 18),
            by_unique(0, 99),
            RowIdentity::default(),
        ];
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
        let one = [(Identifier::MosaicaId, e(3)), (Identifier::Unique(0), e(3))];
        assert_eq!(name_row(&one), Named::One(e(3)));
        let two = [(Identifier::Unique(0), e(3)), (Identifier::Unique(1), e(4))];
        assert_eq!(name_row(&two), Named::Two);
    }

    /// Values naming two items refuse that row and no other, and the refusal says which named
    /// which.
    #[test]
    fn values_naming_two_items_refuse_the_row() {
        let row = RowIdentity {
            mosaica_id: Some(MosaicaId::new(102)),
            unique: vec![(0, 17)],
        };
        assert_eq!(
            resolved(&[RowIdentity::default(), row], Batch::Creates),
            vec![
                Verdict::Creates,
                refused(Refusal::NamesTwo {
                    row: 1,
                    named: vec![(Identifier::MosaicaId, e(2)), (Identifier::Unique(0), e(1))],
                })
            ]
        );
    }

    #[test]
    fn a_mosaica_id_naming_nothing_is_refused() {
        let row = RowIdentity {
            mosaica_id: Some(MosaicaId::new(7)),
            ..RowIdentity::default()
        };
        assert_eq!(
            resolved(&[row], Batch::Creates),
            vec![refused(Refusal::UnknownMosaicaId { row: 0 })]
        );
    }

    /// A refused row claims neither its item nor its values: a row refused for a value an earlier
    /// row set does not make a later row naming its item the second to name it.
    #[test]
    fn a_refused_row_refuses_no_later_row() {
        let row = |a: Key, b: Key| RowIdentity {
            unique: vec![(0, a), (1, b)],
            ..RowIdentity::default()
        };
        let rows = [row(17, 77), row(18, 77), by_unique(0, 18)];
        assert_eq!(
            resolved(&rows, Batch::Creates),
            vec![
                Verdict::Names(e(1)),
                refused(Refusal::OneValueTwice {
                    rows: [0, 1],
                    field: Identifier::Unique(1)
                }),
                Verdict::Names(e(2)),
            ]
        );
        let rows = [by_unique(0, 18), row(18, 77), row(99, 77)];
        assert_eq!(
            resolved(&rows, Batch::Creates),
            vec![
                Verdict::Names(e(2)),
                refused(Refusal::OneItemTwice {
                    rows: [0, 1],
                    item: e(2)
                }),
                Verdict::Creates,
            ]
        );
    }

    /// Two rows naming one item keep the first and refuse the later, and so do two new items
    /// given one value. Nulls are not values and never collide.
    #[test]
    fn the_first_of_two_rows_naming_one_item_or_setting_one_value_is_kept() {
        let by_mosaica = RowIdentity {
            mosaica_id: Some(MosaicaId::new(101)),
            ..RowIdentity::default()
        };
        assert_eq!(
            resolved(&[by_unique(1, 5), by_mosaica], Batch::Creates),
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
                &[
                    by_unique(0, 50),
                    by_unique(1, 6),
                    by_unique(0, 50),
                    by_unique(0, 50)
                ],
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
            resolved(
                &[RowIdentity::default(), RowIdentity::default()],
                Batch::Creates
            ),
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
        let verdicts =
            resolve(&[row(1, 11), row(1, 12), row(3, 12)], &held, Batch::Creates).unwrap();
        assert!(matches!(
            verdicts[0],
            Verdict::Refused(Refusal::NamesTwo { row: 0, .. })
        ));
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
                mosaica_id: Some(MosaicaId::new(7)),
                ..RowIdentity::default()
            },
        ];
        let verdicts = resolved(&rows, Batch::Creates);
        assert_eq!(
            first_refusal(&verdicts),
            Some(&Refusal::UnknownMosaicaId { row: 2 })
        );
    }

    /// Values naming the number in both shards name two items; rows naming it in different shards
    /// name different items.
    #[test]
    fn one_entity_number_in_two_shards_is_two_items() {
        let mut held = Held::default();
        held.unique.insert((0, 17), vec![at(0, 5)]);
        held.unique.insert((1, 9), vec![at(1, 5)]);
        held.mosaica.insert(201, at(1, 5));
        let two = RowIdentity {
            mosaica_id: None,
            unique: vec![(0, 17), (1, 9)],
        };
        assert_eq!(
            resolve(&[two], &held, Batch::Creates).unwrap(),
            vec![refused(Refusal::NamesTwo {
                row: 0,
                named: vec![
                    (Identifier::Unique(0), at(0, 5)),
                    (Identifier::Unique(1), at(1, 5))
                ],
            })]
        );

        let by_mosaica = RowIdentity {
            mosaica_id: Some(MosaicaId::new(201)),
            ..RowIdentity::default()
        };
        let agreeing = RowIdentity {
            mosaica_id: Some(MosaicaId::new(201)),
            unique: vec![(1, 9)],
        };
        assert_eq!(
            resolve(&[by_unique(0, 17), by_mosaica], &held, Batch::Edits).unwrap(),
            vec![Verdict::Names(at(0, 5)), Verdict::Names(at(1, 5))],
            "the number in each shard is its own item"
        );
        assert_eq!(
            resolve(&[by_unique(1, 9), agreeing], &held, Batch::Edits).unwrap(),
            vec![
                Verdict::Names(at(1, 5)),
                refused(Refusal::OneItemTwice {
                    rows: [0, 1],
                    item: at(1, 5)
                })
            ],
            "two rows naming the item in its shard"
        );
    }

    #[test]
    fn a_value_held_outside_the_writers_terms_names_nothing_and_is_never_given_twice() {
        let mut held = held();
        held.outside.push((0, 40));
        let by_mosaica = |id: u64, unique: Vec<(u16, Key)>| RowIdentity {
            mosaica_id: Some(MosaicaId::new(id)),
            unique,
        };
        let rows = [
            by_unique(0, 40),
            by_mosaica(102, vec![(0, 40)]),
            by_unique(0, 41),
        ];
        assert_eq!(
            resolve(&rows, &held, Batch::Creates).unwrap(),
            vec![
                refused(Refusal::ValueHeld {
                    row: 0,
                    field: Identifier::Unique(0),
                    creates: true
                }),
                refused(Refusal::ValueHeld {
                    row: 1,
                    field: Identifier::Unique(0),
                    creates: false
                }),
                Verdict::Creates,
            ]
        );
        assert_eq!(
            resolve(&rows, &held, Batch::Edits).unwrap(),
            vec![
                refused(Refusal::NamesNoItem { row: 0 }),
                refused(Refusal::ValueHeld {
                    row: 1,
                    field: Identifier::Unique(0),
                    creates: false
                }),
                refused(Refusal::NamesNoItem { row: 2 }),
            ]
        );
        assert_eq!(
            resolve(&rows, &held, Batch::Names).unwrap(),
            vec![
                refused(Refusal::NamesNoItem { row: 0 }),
                Verdict::Names(e(2)),
                refused(Refusal::NamesNoItem { row: 2 }),
            ]
        );
    }

    #[test]
    fn a_row_refused_for_a_value_held_outside_refuses_no_later_row() {
        let mut held = held();
        held.outside.push((1, 40));
        let rows = [
            RowIdentity {
                mosaica_id: None,
                unique: vec![(0, 17), (1, 40)],
            },
            by_unique(0, 17),
        ];
        assert_eq!(
            resolve(&rows, &held, Batch::Creates).unwrap(),
            vec![
                refused(Refusal::ValueHeld {
                    row: 0,
                    field: Identifier::Unique(1),
                    creates: false
                }),
                Verdict::Names(e(1)),
            ]
        );
    }
}
