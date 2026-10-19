//! Every write's names, resolved where the write applies.
//!
//! A write names items by `mosaica_id` and by the values of unique fields, under the identity rule
//! a build applies too ([`mosaica_lifecycle::resolve`]). A [`Resolver`] answers that rule against
//! one generation for one writer's [`WriteScope`]. A write's handler resolves its names once, off
//! the write thread, into [`Names`]; the write thread brings them to the generation the write
//! applies in ([`Names::now`]): a row naming an item that has since been deleted, moved by an edit
//! or removed by a fold, and a row carrying a value given a holder since, is resolved again there,
//! and every other row stands as it was resolved. The write thread is the only one that moves an
//! item, so what it resolves holds when the write applies.

use std::collections::VecDeque;
use std::ops::Range;
use std::sync::Arc;

use croaring::Bitmap;
use mosaica_lifecycle::membership::{IncomingArtifact, IncomingGrowth};
use mosaica_lifecycle::resolve::{self, Batch, Reason, Refusal, RowIdentity, Verdict};
use mosaica_lifecycle::ShardSets;
use mosaica_store::manifest::DeclaredScalar;
use mosaica_store::unique::{KeyKind, UniqueKey};
use mosaica_store::StoreError;
use mosaica_types::{EntityId, IdentityKey, ItemAt, LayerSpace, MosaicaId, ShardId, Target};

use crate::control::{AddressTable, AddressValue};
use crate::edited::{NamedTarget, Stamp};
use crate::write::LiveState;
use crate::{EngineError, Generation};

/// Whose terms bound what a write names. [`WriteScope::All`] names every item, as a build does.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub enum WriteScope {
    #[default]
    All,
}

impl WriteScope {
    /// Whether a write of this scope may name `item`.
    fn admits(&self, _item: ItemAt) -> bool {
        match self {
            WriteScope::All => true,
        }
    }
}

/// Who holds what in one generation, for a writer of one scope.
pub(crate) struct Resolver<'a> {
    pub(crate) generation: &'a Generation,
    pub(crate) scope: &'a WriteScope,
    pub(crate) key: &'a IdentityKey,
    pub(crate) live: &'a LiveState,
}

/// The rows a write refused where it applied, by row of its names, with their reasons.
pub type Refused = Vec<(usize, Reason)>;

/// A table's unique columns, each with its declared position, its declaration and its cells.
type UniqueColumns<'a, 't> = Vec<(u16, &'a DeclaredScalar, &'t [Option<AddressValue>])>;

/// What a resolution decided for the rows it was given, in their order, and the columns it
/// ignored.
pub(crate) struct Resolved {
    pub(crate) verdicts: Vec<Result<Target, Refusal>>,
    pub(crate) ignored: Vec<String>,
}

impl<'a> Resolver<'a> {
    pub(crate) fn new(
        generation: &'a Generation,
        scope: &'a WriteScope,
        key: &'a IdentityKey,
        live: &'a LiveState,
    ) -> Resolver<'a> {
        Resolver {
            generation,
            scope,
            key,
            live,
        }
    }

    /// The item each of `ids` names: `None` per position for an identifier that names no item,
    /// an artifact's and a layer's among them. An identifier names a number in its shard, and the
    /// number names the entity the shard's edited-items map holds for it, or the number itself
    /// where it has none ([`crate::edited`]). An item is named while its shard holds it
    /// ([`crate::control::holds_item`]) below the shard's high water and has not deleted it: a fold
    /// that removes a deleted item also drops its deletion, so the overlay alone no longer says it
    /// is gone.
    pub(crate) fn mosaica_items(
        &self,
        ids: &[MosaicaId],
    ) -> Result<Vec<Option<ItemAt>>, StoreError> {
        let by_shard = mosaica_types::by_shard(ids.iter().enumerate().filter_map(|(at, id)| {
            let (shard, number) = crate::edited::number_named(self.key, self.generation, *id)?;
            Some((shard.id(), (at, shard, number)))
        }));
        let mut out = vec![None; ids.len()];
        for (id, named) in by_shard {
            let shard = named[0].1;
            let high_water = self.live.shard(id).map_or(0, |shard| shard.high_water());
            let numbers: Vec<EntityId> = named.iter().map(|(_, _, number)| *number).collect();
            let held = crate::edited::entities_of_numbers(shard, &numbers, |entity| {
                entity.raw() < high_water
                    && crate::control::holds_item(shard, entity)
                    && !shard.overlay.is_deleted(entity)
            })?;
            for ((at, ..), entity) in named.iter().zip(held) {
                out[*at] = entity
                    .map(|entity| ItemAt::new(id, entity))
                    .filter(|item| self.scope.admits(*item));
            }
        }
        Ok(out)
    }

    /// The layer itself, where `entity` is `space`'s own, or an artifact its level holds that the
    /// generation has not deleted.
    pub(crate) fn artifact(&self, space: LayerSpace, entity: EntityId) -> Option<Target> {
        let deleted = self.generation.space_overlay(space).is_deleted(entity);
        (self.live.artifact_held(space, entity) && !deleted)
            .then_some(Target::Artifact(space, entity))
    }

    /// The unique columns of `table` this generation holds an index of, with their cells, and the
    /// other columns, which name nothing.
    fn columns<'t>(&self, table: &'t AddressTable) -> (UniqueColumns<'a, 't>, Vec<String>) {
        let declared = &self.generation.bundle.manifest.declared_scalars;
        let mut unique = Vec::new();
        let mut ignored = Vec::new();
        for (name, cells) in &table.columns {
            let found = declared
                .iter()
                .enumerate()
                .find(|(_, d)| d.name == *name)
                .filter(|_| {
                    self.generation
                        .shards()
                        .any(|shard| shard.unique.get(name).is_some())
                });
            match found {
                Some((at, d)) => unique.push((at as u16, d, cells.as_slice())),
                None => ignored.push(name.clone()),
            }
        }
        (unique, ignored)
    }

    /// Which item each of `rows` of `table` names, every row where `rows` is `None`, under the
    /// identity rule ([`mosaica_lifecycle::resolve`]): a row names the item its `mosaica_id` and
    /// unique values name, and a row naming none, or naming two, is refused. Many rows may name
    /// one item. A column that is neither `mosaica_id` nor a unique field names nothing and is
    /// ignored, as a build ignores it.
    ///
    /// Refused with [`EngineError::AddressMalformed`] where a table with rows is left with no
    /// column to name items by, or a value is not one of its field's.
    pub(crate) fn items(
        &self,
        table: &AddressTable,
        rows: Option<&[usize]>,
    ) -> Result<Resolved, EngineError> {
        let (unique, ignored) = self.columns(table);
        let picked: Vec<usize> = match rows {
            Some(rows) => rows.to_vec(),
            None => (0..table.rows).collect(),
        };
        if picked.is_empty() {
            return Ok(Resolved {
                verdicts: Vec::new(),
                ignored,
            });
        }
        if rows.is_none() {
            resolve::require_identifier(table.mosaica_id.is_some(), unique.len()).map_err(|e| {
                EngineError::AddressMalformed(format!("This request cannot address items: {e}"))
            })?;
        }
        let store = EngineError::Store;
        let no_item = |row| Verdict::Refused(Refusal::NamesNoItem { row });

        // One column alone, the common case, is decided without a row's identity per row.
        let verdicts = match (&table.mosaica_id, unique.as_slice()) {
            (Some(ids), []) => {
                let present: Vec<(usize, MosaicaId)> = picked
                    .iter()
                    .enumerate()
                    .filter_map(|(at, row)| ids[*row].map(|id| (at, id)))
                    .collect();
                let wanted: Vec<MosaicaId> = present.iter().map(|(_, id)| *id).collect();
                let mut verdicts: Vec<Verdict> = (0..picked.len()).map(no_item).collect();
                let found = self.mosaica_items(&wanted).map_err(store)?;
                for ((at, _), item) in present.iter().zip(found) {
                    verdicts[*at] = match item {
                        Some(item) => Verdict::Names(item),
                        None => Verdict::Refused(Refusal::UnknownMosaicaId { row: *at }),
                    };
                }
                verdicts
            }
            (None, [(position, d, cells)]) => {
                let mut keys = Vec::with_capacity(picked.len());
                let mut from = Vec::with_capacity(picked.len());
                for (at, row) in picked.iter().enumerate() {
                    let Some(value) = &cells[*row] else { continue };
                    if let Some(key) = key_of_value(d, value)? {
                        keys.push(key);
                        from.push(at);
                    }
                }
                let mut verdicts: Vec<Verdict> = (0..picked.len()).map(no_item).collect();
                let field = resolve::Identifier::Unique(*position);
                for (at, item) in self.holders_of(d, &keys).map_err(store)? {
                    let row = from[at];
                    verdicts[row] = match &verdicts[row] {
                        Verdict::Names(held) if *held != item => {
                            Verdict::Refused(Refusal::NamesTwo {
                                row,
                                named: vec![(field, *held), (field, item)],
                            })
                        }
                        Verdict::Refused(Refusal::NamesTwo { .. }) => continue,
                        _ => Verdict::Names(item),
                    };
                }
                verdicts
            }
            _ => {
                let mut identities = vec![RowIdentity::default(); picked.len()];
                if let Some(ids) = &table.mosaica_id {
                    for (identity, row) in identities.iter_mut().zip(&picked) {
                        identity.mosaica_id = ids[*row];
                    }
                }
                for (position, d, cells) in &unique {
                    for (identity, row) in identities.iter_mut().zip(&picked) {
                        let Some(value) = &cells[*row] else { continue };
                        if let Some(key) = key_of_value(d, value)? {
                            identity.unique.push((*position, key.widen()));
                        }
                    }
                }
                resolve::resolve(&identities, self, Batch::Names).map_err(store)?
            }
        };
        let verdicts = verdicts
            .into_iter()
            .zip(&picked)
            .map(|(verdict, row)| match verdict {
                Verdict::Names(item) => Ok(item.target()),
                Verdict::Refused(refusal) => Err(at_row(refusal, *row)),
                Verdict::Creates => unreachable!("a call that addresses items creates none"),
            })
            .collect();
        Ok(Resolved { verdicts, ignored })
    }

    /// [`Self::items`] for a call that may also address an artifact or a layer by its
    /// `mosaica_id`, as `/control/changes` does. A row whose `mosaica_id` names an artifact and
    /// whose unique values name an item names two things, and is refused.
    pub(crate) fn targets(
        &self,
        table: &AddressTable,
        rows: Option<&[usize]>,
    ) -> Result<Resolved, EngineError> {
        let picked: Vec<usize> = match rows {
            Some(rows) => rows.to_vec(),
            None => (0..table.rows).collect(),
        };
        let artifact_rows: Vec<(usize, LayerSpace, EntityId)> = picked
            .iter()
            .enumerate()
            .filter_map(|(at, row)| {
                let id = table.mosaica_id.as_ref()?[*row]?;
                let (space, entity) = self.key.invert_artifact(id)?;
                Some((at, space, entity))
            })
            .collect();
        let mut items = table.clone();
        if let Some(ids) = items.mosaica_id.as_mut() {
            for (at, ..) in &artifact_rows {
                ids[picked[*at]] = None;
            }
        }
        let mut resolved = self.items(&items, rows)?;
        for &(at, space, entity) in &artifact_rows {
            let row = picked[at];
            resolved.verdicts[at] = match (&resolved.verdicts[at], self.artifact(space, entity)) {
                (Ok(_), _) => Err(Refusal::NamesTwo {
                    row,
                    named: Vec::new(),
                }),
                (Err(_), Some(target)) => Ok(target),
                (Err(_), None) => Err(Refusal::UnknownMosaicaId { row }),
            };
        }
        Ok(resolved)
    }

    /// The items holding each of `keys` in unique column `declared`.
    fn holders_of(
        &self,
        declared: &DeclaredScalar,
        keys: &[UniqueKey],
    ) -> Result<Vec<(usize, ItemAt)>, StoreError> {
        let mut held = crate::unique::holders(self.generation, &declared.name, keys)?;
        held.retain(|(_, item)| self.scope.admits(*item));
        Ok(held)
    }
}

impl resolve::Holdings for Resolver<'_> {
    type Error = StoreError;

    fn holders(
        &self,
        field: u16,
        keys: &[resolve::Key],
    ) -> Result<Vec<(usize, ItemAt)>, StoreError> {
        let declared = &self.generation.bundle.manifest.declared_scalars[usize::from(field)];
        let kind = KeyKind::of(declared.arrow_type).expect("a unique column's type takes a key");
        let keys: Vec<UniqueKey> = keys
            .iter()
            .map(|k| UniqueKey::of_widened(kind, *k))
            .collect();
        self.holders_of(declared, &keys)
    }

    fn mosaica_holders(&self, ids: &[MosaicaId]) -> Result<Vec<Option<ItemAt>>, StoreError> {
        self.mosaica_items(ids)
    }
}

/// `refusal` as row `row` of the whole table refuses it.
fn at_row(refusal: Refusal, row: usize) -> Refusal {
    match refusal {
        Refusal::NamesTwo { named, .. } => Refusal::NamesTwo { row, named },
        Refusal::UnknownMosaicaId { .. } => Refusal::UnknownMosaicaId { row },
        Refusal::NamesNoItem { .. } => Refusal::NamesNoItem { row },
        other => other,
    }
}

/// A unique value as a caller wrote it, as its field's index keys it.
pub(crate) fn key_of_value(
    declared: &DeclaredScalar,
    value: &AddressValue,
) -> Result<Option<UniqueKey>, EngineError> {
    let keyword = declared.arrow_type == mosaica_spatial::tiler::ScalarType::Keyword;
    let integer = match (value, keyword) {
        (AddressValue::Text(text), true) => return Ok(Some(UniqueKey::keyword(text))),
        (AddressValue::Integer(n), true) => {
            return Err(EngineError::AddressMalformed(format!(
                "{n} is not a value of '{}', which holds keywords. Send it as a string",
                declared.name
            )))
        }
        (AddressValue::Other(shown), _) => {
            return Err(EngineError::AddressMalformed(format!(
                "{shown} is not a value of '{}'. Send {}",
                declared.name,
                match keyword {
                    true => "a string",
                    false => "a whole number",
                }
            )))
        }
        (AddressValue::Integer(n), false) => *n,
        (AddressValue::Text(text), false) => text.parse().map_err(|_| {
            EngineError::AddressMalformed(format!(
                "'{text}' is not a value of '{}', which holds integers. Write it in decimal digits",
                declared.name
            ))
        })?,
    };
    Ok(mosaica_store::unique::key_of_integer(
        declared.arrow_type,
        integer,
    ))
}

/// A unique value of one field, as [`Names`] looks it up.
fn value_hash(field: u16, key: u128) -> u64 {
    use std::hash::{Hash, Hasher};
    let mut hasher = rustc_hash::FxHasher::default();
    (field, key).hash(&mut hasher);
    hasher.finish()
}

/// A write's names: what each row of its table named in the generation its handler resolved it
/// in, and what the write thread needs to resolve again the rows something has touched since.
#[derive(Debug, Clone)]
pub struct Names {
    scope: WriteScope,
    stamp: Stamp,
    table: AddressTable,
    /// Whether a row may name an artifact or a layer by its `mosaica_id`, as a change may.
    targets: bool,
    ignored: Vec<String>,
    verdicts: Vec<Option<Target>>,
    /// The refused rows, ascending.
    refused: Vec<(usize, Refusal)>,
    /// Every item a row's identifiers named, a refused row's among them.
    named: ShardSets,
    /// The same items, each with a row naming it, ascending.
    by_item: Vec<(ItemAt, usize)>,
    /// Each unique value a row carries, as `(value_hash, row)`, ascending.
    valued: Vec<(u64, usize)>,
}

/// A row [`Names::now`] resolved again, and what it named before.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct Moved {
    pub(crate) row: usize,
    pub(crate) was: Option<Target>,
}

impl Names {
    /// `table`'s rows resolved by `resolver`, which may name artifacts and layers where `targets`.
    pub(crate) fn resolve(
        resolver: &Resolver,
        table: AddressTable,
        targets: bool,
    ) -> Result<Names, EngineError> {
        let resolved = match targets {
            true => resolver.targets(&table, None)?,
            false => resolver.items(&table, None)?,
        };
        let mut valued = Vec::new();
        let (unique, _) = resolver.columns(&table);
        for (position, d, cells) in &unique {
            for (row, cell) in cells.iter().enumerate() {
                let key = match cell {
                    Some(value) => key_of_value(d, value)?,
                    None => None,
                };
                if let Some(key) = key {
                    valued.push((value_hash(*position, key.widen()), row));
                }
            }
        }
        valued.sort_unstable();
        let mut names = Names {
            scope: resolver.scope.clone(),
            stamp: Stamp::of(resolver.generation),
            table,
            targets,
            ignored: resolved.ignored,
            verdicts: Vec::new(),
            refused: Vec::new(),
            named: ShardSets::default(),
            by_item: Vec::new(),
            valued,
        };
        names.take((0..names.table.rows).zip(resolved.verdicts));
        Ok(names)
    }

    /// Names for `targets` as resolved at `stamp`, each item addressed by the `mosaica_id` it was
    /// named by: what a caller that resolved entities itself submits.
    pub(crate) fn of_targets(stamp: Stamp, scope: WriteScope, targets: &[NamedTarget]) -> Names {
        let table = AddressTable {
            rows: targets.len(),
            mosaica_id: Some(targets.iter().map(|named| named.item).collect()),
            columns: Vec::new(),
        };
        let mut names = Names {
            scope,
            stamp,
            table,
            targets: true,
            ignored: Vec::new(),
            verdicts: Vec::new(),
            refused: Vec::new(),
            named: ShardSets::default(),
            by_item: Vec::new(),
            valued: Vec::new(),
        };
        names.take(targets.iter().map(|named| Ok(named.target)).enumerate());
        names
    }

    /// Set every row's verdict, and gather the items they name.
    fn take(&mut self, verdicts: impl IntoIterator<Item = (usize, Result<Target, Refusal>)>) {
        for (row, verdict) in verdicts {
            if row >= self.verdicts.len() {
                self.verdicts.resize(row + 1, None);
            }
            match verdict {
                Ok(target) => {
                    if let Target::Item(shard, entity) = target {
                        self.by_item.push((ItemAt::new(shard, entity), row));
                    }
                    self.verdicts[row] = Some(target);
                }
                Err(refusal) => {
                    if let Refusal::NamesTwo { named, .. } = &refusal {
                        self.by_item
                            .extend(named.iter().map(|(_, item)| (*item, row)));
                    }
                    self.verdicts[row] = None;
                    self.refused.push((row, refusal));
                }
            }
        }
        self.refused.sort_by_key(|(row, _)| *row);
        self.by_item.sort_unstable();
        self.by_item.dedup();
        self.named = ShardSets::of_items(self.by_item.iter().map(|(item, _)| *item));
    }

    /// The rows naming `item` by any identifier, as resolved.
    fn rows_naming(&self, item: ItemAt) -> impl Iterator<Item = usize> + '_ {
        let from = self.by_item.partition_point(|(held, _)| *held < item);
        self.by_item[from..]
            .iter()
            .take_while(move |(held, _)| *held == item)
            .map(|(_, row)| *row)
    }

    pub fn rows(&self) -> usize {
        self.table.rows
    }

    /// What row `row` names, or why it names nothing.
    pub fn verdict(&self, row: usize) -> Result<Target, Reason> {
        match self.verdicts[row] {
            Some(target) => Ok(target),
            None => Err(self.refusal(row).map_or(Reason::NamesNoItem, Refusal::kind)),
        }
    }

    fn refusal(&self, row: usize) -> Option<&Refusal> {
        let at = self.refused.binary_search_by_key(&row, |(r, _)| *r).ok()?;
        Some(&self.refused[at].1)
    }

    /// The refused rows, ascending, with their reasons.
    pub fn refused(&self) -> Refused {
        self.refused
            .iter()
            .map(|(row, refusal)| (*row, refusal.kind()))
            .collect()
    }

    /// The columns that name nothing.
    pub fn ignored(&self) -> &[String] {
        &self.ignored
    }

    pub fn stamp(&self) -> Stamp {
        self.stamp
    }

    pub(crate) fn scope(&self) -> &WriteScope {
        &self.scope
    }

    /// The items row `rows` name, as resolved.
    fn items_of(&self, rows: Range<usize>) -> impl Iterator<Item = ItemAt> + '_ {
        self.verdicts[rows]
            .iter()
            .filter_map(|verdict| match verdict {
                Some(Target::Item(shard, entity)) => Some(ItemAt::new(*shard, *entity)),
                _ => None,
            })
    }

    /// Bring the names to `at`'s generation, the one the write applies in: resolve again every
    /// row naming an item `at` has deleted or a fold has removed since the names were resolved,
    /// and every row carrying a value given a holder since; every item row where a fold since is
    /// no longer kept or the unique columns have changed. Answers the rows resolved again,
    /// ascending, with what each named before; every other row names what it named.
    pub(crate) fn now(&mut self, at: &Resolver) -> Result<Vec<Moved>, EngineError> {
        let generation = at.generation;
        let live = &generation.unique_live;
        let retired = generation
            .retirements
            .since(self.stamp.folds, generation.fold_epoch);
        let all = match retired {
            Some(_) => live.columns_changed_since(self.stamp.unique_seq),
            None => true,
        };
        let mut rows: Vec<usize> = match retired {
            Some(retired) if !all => {
                let mut rows = self.touched(generation, &retired);
                rows.extend(self.valued_since(at)?);
                rows
            }
            _ => (0..self.table.rows).collect(),
        };
        // An artifact or a layer never moves, and keeps its entity for ever.
        rows.retain(|row| !matches!(self.verdicts[*row], Some(Target::Artifact(..))));
        rows.sort_unstable();
        rows.dedup();
        if rows.is_empty() {
            return Ok(Vec::new());
        }
        let resolved = match self.targets {
            true => at.targets(&self.table, Some(&rows))?,
            false => at.items(&self.table, Some(&rows))?,
        };
        if all {
            self.ignored = resolved.ignored;
        }
        self.refused
            .retain(|(row, _)| rows.binary_search(row).is_err());
        let mut moved = Vec::with_capacity(rows.len());
        for (row, verdict) in rows.into_iter().zip(resolved.verdicts) {
            moved.push(Moved {
                row,
                was: self.verdicts[row],
            });
            match verdict {
                Ok(target) => self.verdicts[row] = Some(target),
                Err(refusal) => {
                    self.verdicts[row] = None;
                    self.refused.push((row, refusal));
                }
            }
        }
        self.refused.sort_by_key(|(row, _)| *row);
        self.stamp = Stamp::of(generation);
        Ok(moved)
    }

    /// The rows naming an item `generation` has deleted, a fold of `retired` has removed, or
    /// that lies in a shard `generation` no longer holds.
    fn touched(&self, generation: &Generation, retired: &[(ShardId, &Bitmap)]) -> Vec<usize> {
        let mut gone: Vec<(ShardId, Bitmap)> = Vec::new();
        for (id, set) in self.named.iter() {
            let hit = match generation.shard(id) {
                None => set.clone(),
                Some(shard) => {
                    let mut hit = set.and(shard.overlay.deleted_set());
                    for (_, removed) in retired.iter().filter(|(shard, _)| *shard == id) {
                        hit.or_inplace(&set.and(removed));
                    }
                    hit
                }
            };
            if !hit.is_empty() {
                gone.push((id, hit));
            }
        }
        gone.iter()
            .flat_map(|(id, hit)| {
                hit.iter().flat_map(move |entity| {
                    self.rows_naming(ItemAt::new(*id, EntityId::new(u64::from(entity))))
                })
            })
            .collect()
    }

    /// The rows carrying a value given a holder since the names were resolved: every row
    /// carrying a value where an entry added since has left memory.
    fn valued_since(&self, at: &Resolver) -> Result<Vec<usize>, EngineError> {
        let live = &at.generation.unique_live;
        if self.valued.is_empty() || live.seq() <= self.stamp.unique_seq {
            return Ok(Vec::new());
        }
        if live.stale_since(self.stamp.unique_seq) {
            return Ok(self.valued.iter().map(|(_, row)| *row).collect());
        }
        let mut rows = Vec::new();
        let (unique, _) = at.columns(&self.table);
        for (position, d, _) in unique {
            for key in live.keys_added_since(&d.name, self.stamp.unique_seq) {
                let hash = value_hash(position, key.widen());
                let from = self.valued.partition_point(|(h, _)| *h < hash);
                rows.extend(
                    self.valued[from..]
                        .iter()
                        .take_while(|(h, _)| *h == hash)
                        .map(|(_, row)| *row),
                );
            }
        }
        Ok(rows)
    }
}

/// Which set of an artifact a member table fills.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MemberSet {
    /// A publication's `members`, or a growth's joining members.
    Members,
    Excluding,
    /// The generating set of the publication's content at this position in its contents.
    GeneratedFrom(usize),
    /// A growth's leaving members.
    Leaving,
}

/// One member table of a write, as the rows of its [`Names`] it is.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct NamedList {
    /// The position of the artifact the table belongs to in the write.
    pub artifact: usize,
    pub set: MemberSet,
    pub rows: Range<usize>,
    /// A refused row refuses the write.
    pub whole: bool,
}

/// A publication's or a growth's member tables, resolved as one table.
#[derive(Debug, Clone)]
pub struct MemberNames {
    pub(crate) names: Names,
    pub(crate) lists: Vec<NamedList>,
}

impl MemberNames {
    pub fn new(names: Names, lists: Vec<NamedList>) -> MemberNames {
        MemberNames { names, lists }
    }

    pub fn names(&self) -> &Names {
        &self.names
    }

    /// [`Names::now`] over the tables' rows.
    pub(crate) fn now(&mut self, at: &Resolver) -> Result<Vec<Moved>, EngineError> {
        self.names.now(at)
    }

    /// The set `list` fills: built from its rows where `moved` is `None`, and otherwise `held`,
    /// the set built before, less each item a moved row no longer names and no other row of the
    /// list names, and with each item a moved row names now.
    fn set_of(&self, list: &NamedList, moved: Option<&[Moved]>, held: &mut ShardSets) {
        let Some(moved) = moved else {
            *held = ShardSets::of_items(self.names.items_of(list.rows.clone()));
            return;
        };
        let from = moved.partition_point(|m| m.row < list.rows.start);
        let within: Vec<&Moved> = moved[from..]
            .iter()
            .take_while(|m| m.row < list.rows.end)
            .collect();
        if within.is_empty() {
            return;
        }
        let moved_row = |row: usize| within.binary_search_by_key(&row, |m| m.row).is_ok();
        let mut removed = Vec::new();
        let mut added = Vec::new();
        for m in &within {
            if let Some(Target::Item(shard, entity)) = m.was {
                let item = ItemAt::new(shard, entity);
                let kept = self.names.rows_naming(item).any(|row| {
                    list.rows.contains(&row)
                        && !moved_row(row)
                        && self.names.verdicts[row] == Some(item.target())
                });
                if !kept {
                    removed.push(item);
                }
            }
            if let Ok(Target::Item(shard, entity)) = self.names.verdict(m.row) {
                added.push(ItemAt::new(shard, entity));
            }
        }
        let shards: std::collections::BTreeSet<ShardId> = removed
            .iter()
            .chain(&added)
            .map(|item| item.shard)
            .collect();
        for shard in shards {
            let mut set = held.get(shard).cloned().unwrap_or_default();
            for item in removed.iter().filter(|item| item.shard == shard) {
                set.remove(item.entity.raw() as u32);
            }
            for item in added.iter().filter(|item| item.shard == shard) {
                set.add(item.entity.raw() as u32);
            }
            held.set(shard, set);
        }
    }

    /// Each list's set, as [`Self::set_of`] makes it.
    pub(crate) fn fill_artifacts(
        &self,
        artifacts: &mut [IncomingArtifact],
        moved: Option<&[Moved]>,
    ) {
        for list in &self.lists {
            let Some(artifact) = artifacts.get_mut(list.artifact) else {
                continue;
            };
            match list.set {
                MemberSet::Members => self.set_of(list, moved, &mut artifact.members),
                MemberSet::Excluding => self.set_of(
                    list,
                    moved,
                    artifact.excluding.get_or_insert_with(Default::default),
                ),
                MemberSet::GeneratedFrom(at) => {
                    if let Some(content) = artifact.contents.get_mut(at) {
                        self.set_of(list, moved, &mut content.generated_from);
                    }
                }
                MemberSet::Leaving => {}
            }
        }
    }

    /// [`Self::fill_artifacts`] for a growth's joins.
    pub(crate) fn fill_growth(&self, joins: &mut [IncomingGrowth], moved: Option<&[Moved]>) {
        for list in &self.lists {
            let Some(join) = joins.get_mut(list.artifact) else {
                continue;
            };
            match list.set {
                MemberSet::Members => self.set_of(list, moved, &mut join.joining),
                MemberSet::Leaving => self.set_of(list, moved, &mut join.leaving),
                MemberSet::Excluding | MemberSet::GeneratedFrom(_) => {}
            }
        }
    }

    /// The first refused row of a list a refused row refuses whole, with its reason.
    pub(crate) fn refusing(&self) -> Option<(usize, Reason)> {
        self.names
            .refused
            .iter()
            .find(|(row, _)| {
                self.lists
                    .iter()
                    .any(|list| list.whole && list.rows.contains(row))
            })
            .map(|(row, refusal)| (*row, refusal.kind()))
    }

    /// Names for the members of each set `lists` yields, as `generation` holds them, each
    /// addressed by its `mosaica_id`: what a caller that resolved entities itself submits.
    pub(crate) fn of_sets<'s>(
        key: &IdentityKey,
        generation: &Generation,
        lists: impl IntoIterator<Item = (usize, MemberSet, bool, &'s ShardSets)>,
    ) -> Result<MemberNames, StoreError> {
        let mut targets = Vec::new();
        let mut named = Vec::new();
        for (artifact, set, whole, sets) in lists {
            let start = targets.len();
            for (shard, members) in sets.iter() {
                targets.extend(
                    members
                        .iter()
                        .map(|e| Target::Item(shard, EntityId::new(u64::from(e)))),
                );
            }
            named.push(NamedList {
                artifact,
                set,
                rows: start..targets.len(),
                whole,
            });
        }
        let resolved = NamedTarget::all(key, generation, &targets)?;
        Ok(MemberNames {
            names: Names::of_targets(Stamp::of(generation), WriteScope::All, &resolved),
            lists: named,
        })
    }
}

/// The entities the latest folds removed, by the fold epoch each published at: how the write
/// thread finds the items a fold has removed since a write's names were resolved.
#[derive(Debug, Clone, Default)]
pub(crate) struct Retirements(VecDeque<(u64, ShardId, Arc<Bitmap>)>);

/// How many folds' removals are kept. A write whose names were resolved before the oldest kept
/// one has every row resolved again.
const RETIREMENTS_KEPT: usize = 16;

impl Retirements {
    /// These, and the entities of `shard` the fold publishing at `epoch` removed.
    pub(crate) fn with(&self, epoch: u64, shard: ShardId, retired: &Bitmap) -> Retirements {
        let mut kept = self.0.clone();
        kept.push_back((epoch, shard, Arc::new(retired.clone())));
        while kept.len() > RETIREMENTS_KEPT {
            kept.pop_front();
        }
        Retirements(kept)
    }

    /// What every fold after epoch `since`, through `now`, removed, or `None` where one of them is
    /// no longer kept.
    pub(crate) fn since(&self, since: u64, now: u64) -> Option<Vec<(ShardId, &Bitmap)>> {
        let after: Vec<(ShardId, &Bitmap)> = self
            .0
            .iter()
            .filter(|(epoch, ..)| *epoch > since)
            .map(|(_, shard, retired)| (*shard, retired.as_ref()))
            .collect();
        (after.len() as u64 == now.saturating_sub(since)).then_some(after)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn bitmap(entities: &[u32]) -> Bitmap {
        entities.iter().copied().collect()
    }

    /// A write resolved before a fold finds what the fold removed while the fold is kept, and is
    /// resolved again whole once it is not.
    #[test]
    fn a_fold_since_a_write_was_named_is_answered_while_it_is_kept() {
        let mut kept = Retirements::default();
        assert_eq!(kept.since(0, 0).map(|r| r.len()), Some(0), "no fold since");
        for epoch in 1..=RETIREMENTS_KEPT as u64 + 2 {
            kept = kept.with(epoch, ShardId::ZERO, &bitmap(&[epoch as u32]));
        }
        let now = RETIREMENTS_KEPT as u64 + 2;
        let since = kept.since(now - 1, now).expect("the last fold is kept");
        assert_eq!(since.len(), 1);
        assert!(since[0].1.contains(now as u32));
        assert_eq!(kept.since(now, now).map(|r| r.len()), Some(0));
        assert!(
            kept.since(1, now).is_none(),
            "the fold after epoch 1 is no longer kept"
        );
        assert_eq!(
            kept.since(2, now).map(|r| r.len()),
            Some(RETIREMENTS_KEPT),
            "every kept fold"
        );
    }
}
