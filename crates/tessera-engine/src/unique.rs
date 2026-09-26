//! Unique fields at a running service: the index entries of values no run holds yet, and the one
//! lookup every reader of a unique column goes through.
//!
//! A unique column's values reach its index in two steps. A row accepted into the ingest buffer
//! (or a values fill) adds a live entry here at its window's close; the flush that writes the row
//! writes a run holding the same entries, and its publication removes them from here. A lookup
//! gathers the runs' entities and the live entries' and drops deleted entities, so a value is
//! found from its acknowledgement onwards whichever of the two holds it.
//!
//! Every entry carries the sequence number it was added under. A handler that checked a batch
//! against one generation sends that generation's number with it, and the executor re-checks the
//! batch against the entries added since. The entries the last few flushes moved into runs are
//! kept beside the live ones for that re-check, so it stays complete across a flush.
//! [`UniqueLive::stale_since`] answers where an entry added after the handler's number has left
//! even those, or where the set of unique columns has changed, and the handler then checks the
//! batch again.

use std::collections::BTreeMap;
use std::sync::Arc;

use rustc_hash::FxHashMap;
use tessera_lifecycle::{IngestBuffer, WalScalar};
use tessera_store::manifest::{DeclaredScalar, Manifest};
use tessera_store::unique::{key_of, UniqueKey};
use tessera_types::EntityId;

use crate::Generation;

/// One entity holding a key, and the sequence number the entry was added under.
type Entry = (EntityId, u64);

/// The entries one flush moved into runs, by attribute.
type Retired = BTreeMap<String, FxHashMap<UniqueKey, Vec<Entry>>>;

/// How many moved entries are kept for the executor's re-check, across the flushes that moved
/// them. The newest flush's are kept whatever their number.
const RETIRED_MAX_ENTRIES: usize = 1 << 20;

/// The index entries no run holds yet, per unique column.
#[derive(Debug, Clone, Default)]
pub(crate) struct UniqueLive {
    /// Keyed by attribute name. An `Arc` per column, so a window adding to one column copies that
    /// column's map and shares the rest.
    columns: BTreeMap<String, Arc<FxHashMap<UniqueKey, Vec<Entry>>>>,
    /// The number the last entry was added under.
    seq: u64,
    /// The entries the latest flushes moved into runs, oldest first, with how many they hold.
    retired: std::collections::VecDeque<(Arc<Retired>, usize)>,
    /// The highest number among the moved entries no longer kept in `retired`.
    flushed_through: u64,
    /// The number current when the set of unique columns last changed.
    columns_changed_at: u64,
}

impl UniqueLive {
    /// The live entries of every unique column `manifest` declares, from the rows and fills
    /// `buffer` holds: what a restart or a declaration starts from.
    pub(crate) fn derive(manifest: &Manifest, buffer: &IngestBuffer) -> UniqueLive {
        let mut live = UniqueLive::default();
        for declared in manifest.declared_scalars.iter().filter(|d| d.unique) {
            live.columns.insert(declared.name.clone(), Arc::default());
        }
        live.add_buffered(manifest, buffer, None);
        live
    }

    /// Add the entries of every row and fill `buffer` holds, for `only` or every unique column.
    fn add_buffered(&mut self, manifest: &Manifest, buffer: &IngestBuffer, only: Option<&str>) {
        let mut entities: Vec<(EntityId, &[WalScalar])> = buffer
            .iter()
            .map(|(entity, item)| (*entity, item.scalars.as_slice()))
            .chain(
                buffer
                    .fills()
                    .map(|(entity, fill)| (*entity, fill.scalars.as_slice())),
            )
            .collect();
        entities.sort_by_key(|(entity, _)| *entity);
        for (entity, scalars) in entities {
            for (at, declared) in manifest.declared_scalars.iter().enumerate() {
                let wanted = match only {
                    Some(name) => name == declared.name,
                    None => declared.unique,
                };
                if !wanted {
                    continue;
                }
                if let Some(key) = scalars.get(at).and_then(|v| key_of(declared.arrow_type, v)) {
                    self.add(&declared.name, key, entity);
                }
            }
        }
    }

    /// The sequence number of the last entry added: what a handler checked a batch at.
    pub(crate) fn seq(&self) -> u64 {
        self.seq
    }

    /// Whether an entry added after `seq` may be missing from here, or the unique columns have
    /// changed since `seq`, so a batch checked at `seq` cannot be re-checked from memory.
    pub(crate) fn stale_since(&self, seq: u64) -> bool {
        self.flushed_through > seq || self.columns_changed_at > seq
    }

    /// Whether no column is unique, so nothing is ever added.
    pub(crate) fn is_empty_schema(&self) -> bool {
        self.columns.is_empty()
    }

    /// Record that `entity` holds `key` in `attribute`. Does nothing for a column that is not
    /// unique.
    pub(crate) fn add(&mut self, attribute: &str, key: UniqueKey, entity: EntityId) {
        let Some(column) = self.columns.get_mut(attribute) else {
            return;
        };
        self.seq += 1;
        let entries = Arc::make_mut(column).entry(key).or_default();
        if !entries.iter().any(|(held, _)| *held == entity) {
            entries.push((entity, self.seq));
        }
    }

    /// Add the entries of one row's or fill's entity-scoped scalars.
    pub(crate) fn add_scalars(
        &mut self,
        declared: &[DeclaredScalar],
        entity: EntityId,
        scalars: &[WalScalar],
    ) {
        for (at, d) in declared.iter().enumerate() {
            if !d.unique {
                continue;
            }
            if let Some(key) = scalars.get(at).and_then(|v| key_of(d.arrow_type, v)) {
                self.add(&d.name, key, entity);
            }
        }
    }

    /// Move the entries one flush wrote into runs, per attribute, from the live entries to the
    /// kept ones, forgetting the oldest kept flushes past [`RETIRED_MAX_ENTRIES`].
    pub(crate) fn flushed<'a>(
        &mut self,
        written: impl IntoIterator<Item = (&'a str, &'a [(UniqueKey, EntityId)])>,
    ) {
        let mut moved: Retired = BTreeMap::new();
        let mut count = 0usize;
        for (attribute, entries_written) in written {
            let Some(column) = self.columns.get_mut(attribute) else {
                continue;
            };
            let column = Arc::make_mut(column);
            let into = moved.entry(attribute.to_string()).or_default();
            for (key, entity) in entries_written {
                let Some(entries) = column.get_mut(key) else {
                    continue;
                };
                if let Some(i) = entries.iter().position(|(held, _)| held == entity) {
                    into.entry(*key).or_default().push(entries.swap_remove(i));
                    count += 1;
                }
                if entries.is_empty() {
                    column.remove(key);
                }
            }
        }
        if count == 0 {
            return;
        }
        self.retired.push_back((Arc::new(moved), count));
        let mut held: usize = self.retired.iter().map(|(_, n)| n).sum();
        while held > RETIRED_MAX_ENTRIES && self.retired.len() > 1 {
            let (oldest, n) = self.retired.pop_front().expect("more than one held");
            held -= n;
            for entries in oldest.values().flat_map(|column| column.values()) {
                for (_, seq) in entries {
                    self.flushed_through = self.flushed_through.max(*seq);
                }
            }
        }
    }

    /// Remove every entry of the deleted entities. A deleted entity names nothing, so this changes
    /// no answer; it only returns the memory.
    pub(crate) fn remove_entities(&mut self, deleted: &[EntityId]) {
        if deleted.is_empty() {
            return;
        }
        for column in self.columns.values_mut() {
            if column.values().all(|entries| entries.iter().all(|(e, _)| !deleted.contains(e))) {
                continue;
            }
            let column = Arc::make_mut(column);
            column.retain(|_, entries| {
                entries.retain(|(e, _)| !deleted.contains(e));
                !entries.is_empty()
            });
        }
    }

    /// Make `attribute` unique, with the entries of every row and fill `buffer` holds, or stop it
    /// being unique.
    pub(crate) fn set_unique(
        &mut self,
        manifest: &Manifest,
        buffer: &IngestBuffer,
        attribute: &str,
        unique: bool,
    ) {
        if unique {
            self.columns.insert(attribute.to_string(), Arc::default());
            self.add_buffered(manifest, buffer, Some(attribute));
        } else {
            self.columns.remove(attribute);
        }
        // Past every number a handler can have read, including one that saw no entry added.
        self.seq += 1;
        self.columns_changed_at = self.seq;
    }

    /// Every entity a live entry of `attribute` holds under each of `keys`, as `(position in keys,
    /// entity)`.
    pub(crate) fn lookup(&self, attribute: &str, keys: &[UniqueKey]) -> Vec<(usize, EntityId)> {
        let mut out = Vec::new();
        if let Some(column) = self.columns.get(attribute) {
            collect(column, keys, 0, &mut out);
        }
        out
    }

    /// [`Self::lookup`] over the entries added after `after`, the live ones and the kept ones a
    /// flush has moved into runs: the executor's re-check.
    pub(crate) fn added_since(
        &self,
        attribute: &str,
        keys: &[UniqueKey],
        after: u64,
    ) -> Vec<(usize, EntityId)> {
        let mut out = Vec::new();
        if let Some(column) = self.columns.get(attribute) {
            collect(column, keys, after, &mut out);
        }
        for (retired, _) in &self.retired {
            if let Some(column) = retired.get(attribute) {
                collect(column, keys, after, &mut out);
            }
        }
        out
    }
}

fn collect(
    column: &FxHashMap<UniqueKey, Vec<Entry>>,
    keys: &[UniqueKey],
    after: u64,
    out: &mut Vec<(usize, EntityId)>,
) {
    for (i, key) in keys.iter().enumerate() {
        for (entity, seq) in column.get(key).into_iter().flatten() {
            if *seq > after {
                out.push((i, *entity));
            }
        }
    }
}

/// The entities of `generation` holding each of `keys` in unique column `attribute`, from its
/// runs and its live entries, deleted entities dropped, as `(position in keys, entity)` ascending
/// by position. A suppressed entity holds its value. Nothing here asks who may see a holder: a
/// caller answering a viewer intersects with the viewer's visible set.
pub(crate) fn holders(
    generation: &Generation,
    attribute: &str,
    keys: &[UniqueKey],
) -> Result<Vec<(usize, EntityId)>, tessera_store::StoreError> {
    let mut out: Vec<(usize, EntityId)> = match generation.unique.get(attribute) {
        Some(index) => index
            .lookup(keys)?
            .into_iter()
            .map(|(i, entity)| (i, EntityId::new(u64::from(entity))))
            .collect(),
        None => Vec::new(),
    };
    out.extend(generation.unique_live.lookup(attribute, keys));
    out.retain(|(_, entity)| !generation.overlay.is_deleted(*entity));
    out.sort_unstable();
    out.dedup();
    Ok(out)
}

/// The unique keys one row sets, as `(declared position, key)`, for a row that creates an item.
pub(crate) fn row_keys(declared: &[DeclaredScalar], scalars: &[WalScalar]) -> Vec<(usize, UniqueKey)> {
    declared
        .iter()
        .enumerate()
        .filter(|(_, d)| d.unique)
        .filter_map(|(at, d)| {
            scalars
                .get(at)
                .and_then(|v| key_of(d.arrow_type, v))
                .map(|key| (at, key))
        })
        .collect()
}

/// A value as a refusal names it: a keyword quoted, a number as its digits.
pub(crate) fn describe(value: &WalScalar) -> String {
    match value {
        WalScalar::Utf8(s) => format!("'{s}'"),
        WalScalar::U8(v) => v.to_string(),
        WalScalar::U16(v) => v.to_string(),
        WalScalar::U32(v) => v.to_string(),
        WalScalar::U64(v) => v.to_string(),
        WalScalar::I8(v) => v.to_string(),
        WalScalar::I16(v) => v.to_string(),
        WalScalar::I32(v) => v.to_string(),
        WalScalar::I64(v) | WalScalar::TimestampUs(v) => v.to_string(),
        other => format!("{other:?}"),
    }
}

/// An item's `tessera_id` as a refusal names it.
pub(crate) fn tessera_id_text(
    key: &tessera_types::IdentityKey,
    generation: &Generation,
    entity: EntityId,
) -> String {
    key.forward(generation.bundle.manifest.identity.shard_id, entity)
        .map(|id| id.raw().to_string())
        .unwrap_or_else(|_| "unknown".to_string())
}

/// The unique values a batch's rows set, as a commit window holds them.
pub(crate) fn window_keys(
    declared: &[DeclaredScalar],
    rows: &[tessera_lifecycle::UnallocatedRow],
) -> Vec<(u16, u128)> {
    rows.iter()
        .filter(|row| row.join.is_none())
        .flat_map(|row| row_keys(declared, &row.scalars))
        .map(|(at, key)| (u16::try_from(at).unwrap_or(u16::MAX), key.widen()))
        .collect()
}

/// One row's value for one unique column, and the item the row names, if it joins or fills one.
struct Setting<'a> {
    row: usize,
    target: Option<EntityId>,
    key: UniqueKey,
    value: &'a WalScalar,
}

/// Every unique column a batch sets, with the settings of its rows.
type Settings<'a> = Vec<(&'a DeclaredScalar, Vec<Setting<'a>>)>;

/// What an ingest batch's rows set in each unique column.
fn row_settings<'a>(
    declared: &'a [DeclaredScalar],
    rows: &'a [tessera_lifecycle::UnallocatedRow],
) -> Settings<'a> {
    declared
        .iter()
        .enumerate()
        .filter(|(_, d)| d.unique)
        .map(|(at, d)| {
            let set = rows
                .iter()
                .enumerate()
                .filter_map(|(row, r)| {
                    let value = r.scalars.get(at)?;
                    key_of(d.arrow_type, value).map(|key| Setting {
                        row,
                        target: r.join,
                        key,
                        value,
                    })
                })
                .collect();
            (d, set)
        })
        .collect()
}

/// What a values batch's rows fill in each unique column it names.
fn fill_settings<'a>(
    declared: &'a [DeclaredScalar],
    request: &'a tessera_lifecycle::ValuesRequest,
) -> Settings<'a> {
    request
        .columns
        .iter()
        .enumerate()
        .filter_map(|(position, name)| {
            let d = declared.iter().find(|d| d.unique && d.name == *name)?;
            let set = request
                .rows
                .iter()
                .enumerate()
                .filter_map(|(row, r)| {
                    let value = r.values.get(position)?;
                    key_of(d.arrow_type, value).map(|key| Setting {
                        row,
                        target: Some(r.entity),
                        key,
                        value,
                    })
                })
                .collect();
            Some((d, set))
        })
        .collect()
}

fn taken(
    d: &DeclaredScalar,
    setting: &Setting<'_>,
    holder: EntityId,
    identity: &tessera_types::IdentityKey,
    generation: &Generation,
) -> tessera_lifecycle::ExecError {
    let id = tessera_id_text(identity, generation, holder);
    tessera_lifecycle::ExecError::UniqueTaken {
        detail: format!(
            "row {}: '{}' = {} is held by item {id}; a unique value is held by one item, so send \
             another value, or delete item {id} first",
            setting.row,
            d.name,
            describe(setting.value)
        ),
    }
}

/// Refuse settings that set one value on two items, or a value an item other than the row's own
/// holds, against `generation`'s runs and live entries. Answers the live entries' sequence
/// number the check was made at, for the executor's re-check.
fn check_settings(
    generation: &Generation,
    settings: Settings<'_>,
    identity: &tessera_types::IdentityKey,
) -> Result<u64, crate::write::AcceptError> {
    use crate::write::AcceptError;
    let seq = generation.unique_live.seq();
    for (d, set) in settings {
        if set.is_empty() {
            continue;
        }
        let mut seen: FxHashMap<UniqueKey, usize> = FxHashMap::default();
        for (i, setting) in set.iter().enumerate() {
            let Some(j) = seen.insert(setting.key, i) else {
                continue;
            };
            let first = &set[j];
            if first.target.is_none() || first.target != setting.target {
                return Err(AcceptError::Exec(tessera_lifecycle::ExecError::UniqueTaken {
                    detail: format!(
                        "rows {} and {} both set '{}' = {}; a unique value is held by one item, \
                         so send it in one row",
                        first.row,
                        setting.row,
                        d.name,
                        describe(setting.value)
                    ),
                }));
            }
        }
        let keys: Vec<UniqueKey> = set.iter().map(|s| s.key).collect();
        let found = holders(generation, &d.name, &keys)
            .map_err(|e| AcceptError::UniqueIndexUnreadable(e.to_string()))?;
        if let Some((i, holder)) = found
            .into_iter()
            .find(|(i, holder)| set[*i].target != Some(*holder))
        {
            return Err(AcceptError::Exec(taken(d, &set[i], holder, identity, generation)));
        }
    }
    Ok(seq)
}

/// Why the executor's re-check did not pass.
pub(crate) enum Recheck {
    /// A value is held by another item, or two items would hold it.
    Refused(tessera_lifecycle::ExecError),
    /// What was added since the handler's check is not all in memory, so the handler checks
    /// again.
    Stale,
}

/// The executor's half of a check: the settings against the live entries added after `seq`,
/// from memory.
fn recheck_settings(
    generation: &Generation,
    settings: Settings<'_>,
    seq: u64,
    identity: &tessera_types::IdentityKey,
) -> Result<(), Recheck> {
    if settings.iter().all(|(_, set)| set.is_empty()) {
        return Ok(());
    }
    if generation.unique_live.stale_since(seq) {
        return Err(Recheck::Stale);
    }
    for (d, set) in settings {
        if set.is_empty() {
            continue;
        }
        let keys: Vec<UniqueKey> = set.iter().map(|s| s.key).collect();
        let found = generation.unique_live.added_since(&d.name, &keys, seq);
        if let Some((i, holder)) = found.into_iter().find(|(i, holder)| {
            set[*i].target != Some(*holder) && !generation.overlay.is_deleted(*holder)
        }) {
            return Err(Recheck::Refused(taken(d, &set[i], holder, identity, generation)));
        }
    }
    Ok(())
}

/// Refuse an ingest batch whose rows set one unique value twice, or a value an item other than
/// the row's own holds. Answers the sequence number to re-check from.
pub(crate) fn check_rows(
    generation: &Generation,
    rows: &[tessera_lifecycle::UnallocatedRow],
    identity: &tessera_types::IdentityKey,
) -> Result<u64, crate::write::AcceptError> {
    let declared = &generation.bundle.manifest.declared_scalars;
    check_settings(generation, row_settings(declared, rows), identity)
}

/// [`check_rows`] on the executor, against what was added after `seq`.
pub(crate) fn recheck_rows(
    generation: &Generation,
    rows: &[tessera_lifecycle::UnallocatedRow],
    seq: u64,
    identity: &tessera_types::IdentityKey,
) -> Result<(), Recheck> {
    let declared = &generation.bundle.manifest.declared_scalars;
    recheck_settings(generation, row_settings(declared, rows), seq, identity)
}

/// Refuse a values batch whose fills set one unique value on two items, or a value another live
/// or suppressed item holds. Answers the sequence number to re-check from.
pub(crate) fn check_fills(
    generation: &Generation,
    request: &tessera_lifecycle::ValuesRequest,
    identity: &tessera_types::IdentityKey,
) -> Result<u64, crate::write::AcceptError> {
    let declared = &generation.bundle.manifest.declared_scalars;
    check_settings(generation, fill_settings(declared, request), identity)
}

/// [`check_fills`] on the executor, against what was added after `seq`.
pub(crate) fn recheck_fills(
    generation: &Generation,
    request: &tessera_lifecycle::ValuesRequest,
    seq: u64,
    identity: &tessera_types::IdentityKey,
) -> Result<(), Recheck> {
    let declared = &generation.bundle.manifest.declared_scalars;
    recheck_settings(generation, fill_settings(declared, request), seq, identity)
}

/// Every value `generation` has flushed for column `at` among `wanted`, to `visit`, deleted
/// entities left out, read from the column's own home: its value layers where it has them, the
/// record blob a block at a time where it is blob-resident, and each view's row tail where it is
/// rendered alone.
pub(crate) fn for_each_flushed_value(
    generation: &Generation,
    at: usize,
    wanted: &croaring::Bitmap,
    visit: &mut dyn FnMut(u32, WalScalar) -> Result<(), String>,
) -> Result<(), String> {
    let manifest = &generation.bundle.manifest;
    let declared = &manifest.declared_scalars[at];
    let mut wanted = wanted.clone();
    wanted.andnot_inplace(generation.overlay.deleted_set());
    let mut failed: Option<String> = None;
    let mut send = |entity: u32, value: WalScalar| {
        if failed.is_none() {
            if let Err(e) = visit(entity, value) {
                failed = Some(e);
            }
        }
    };
    if crate::filter::owes_value_column(declared, &manifest.vocabularies) {
        let columns = &generation.filter_columns;
        let mut present = croaring::Bitmap::new();
        if let Some(layers) = columns.value_layers(&declared.name) {
            for layer in layers.base().into_iter().chain(layers.extents()) {
                present |= layer.present_in(&wanted);
            }
        }
        for entity in present.iter() {
            if let Some(value) = columns
                .stored_value(&declared.name, entity)
                .and_then(|v| crate::write::joined::stored_as_wal(v, declared))
            {
                send(entity, value);
            }
        }
    } else if crate::filter::blob_resident(declared, &manifest.vocabularies) {
        generation
            .filter_columns
            .records()
            .for_each_row_in(&wanted, &mut |entity, fields| {
                let value = fields
                    .into_iter()
                    .find(|field| field.tag as usize == at)
                    .and_then(|field| crate::write::joined::stored_as_wal(field.value, declared));
                if let Some(value) = value {
                    send(entity, value);
                }
                Ok(())
            })
            .map_err(|e| format!("the record blob could not be read: {e}"))?;
    } else {
        // Rendered alone: every view's rows carry the value, so each entity is read from the
        // first view that holds it.
        for partition in generation.bundle.partitions.values() {
            for view in partition.views.values() {
                let mut read = croaring::Bitmap::new();
                view.for_each_rendered(&declared.name, &wanted, &mut |entity, value| {
                    read.add(entity);
                    send(entity, value);
                })?;
                wanted.andnot_inplace(&read);
            }
        }
    }
    failed.map_or(Ok(()), Err)
}

/// How many values one refusal names at most.
pub(crate) const EXAMPLES: usize = 10;

/// What one round of a declaration's build is given: the generation it reads, the flushed
/// entities it reads the column's values for, the runs earlier rounds wrote, and where it writes.
pub(crate) struct RoundInput {
    pub(crate) generation: Arc<Generation>,
    pub(crate) at: usize,
    pub(crate) wanted: croaring::Bitmap,
    pub(crate) prior: Vec<std::path::PathBuf>,
    pub(crate) prefix_dir: std::path::PathBuf,
    pub(crate) out_dir: std::path::PathBuf,
    pub(crate) stem: String,
    pub(crate) memory_budget: usize,
}

/// What a round wrote and found.
pub(crate) struct RoundOutput {
    pub(crate) runs: Vec<(tessera_store::unique::WrittenUniqueRun, tessera_store::manifest::FileDigest)>,
    /// Keys more than one live or suppressed item holds, and up to [`EXAMPLES`] of their values.
    pub(crate) duplicates: u64,
    pub(crate) examples: Vec<String>,
    /// The buffered `(entity, key)` pairs this round checked against every run.
    pub(crate) checked: FxHashMap<EntityId, UniqueKey>,
}

/// One round of a unique declaration's build, off the executor: sort the flushed values of the
/// wanted entities into runs, then find every key more than one item holds, among them, against
/// the earlier rounds' runs, and among and against the buffered rows.
pub(crate) fn build_round(input: RoundInput) -> Result<RoundOutput, String> {
    use tessera_store::unique::{KeyKind, UniqueIndex, UniqueSpill};
    let generation = &input.generation;
    let declared = &generation.bundle.manifest.declared_scalars[input.at];
    let ty = declared.arrow_type;
    let kind = KeyKind::of(ty).ok_or_else(|| format!("'{}' cannot be unique", declared.name))?;
    let deleted = |entity: EntityId| generation.overlay.is_deleted(entity);

    std::fs::create_dir_all(&input.out_dir).map_err(|e| e.to_string())?;
    let mut spill = UniqueSpill::create(kind, &input.out_dir, input.memory_budget)
        .map_err(|e| e.to_string())?;
    for_each_flushed_value(generation, input.at, &input.wanted, &mut |entity, value| {
        match key_of(ty, &value) {
            Some(key) => spill.push(key, entity).map_err(|e| e.to_string()),
            None => Ok(()),
        }
    })?;
    let mut duplicate_keys: Vec<UniqueKey> = Vec::new();
    let mut duplicates = 0u64;
    let written = spill
        .finish(&input.out_dir, &input.stem, |d| {
            duplicates += 1;
            if duplicate_keys.len() < EXAMPLES {
                duplicate_keys.push(d.key);
            }
        })
        .map_err(|e| e.to_string())?;
    let note = |key: UniqueKey, duplicates: &mut u64, keys: &mut Vec<UniqueKey>| {
        if !keys.contains(&key) {
            *duplicates += 1;
            if keys.len() < EXAMPLES {
                keys.push(key);
            }
        }
    };

    // Every run so far, this round's and the earlier ones', as one index to look up in.
    let rel = |path: &std::path::Path| {
        tessera_store::unique::relative(&input.prefix_dir, path).map_err(|e| e.to_string())
    };
    let this_round: Vec<String> = written
        .iter()
        .map(|run| rel(&run.path))
        .collect::<Result<_, _>>()?;
    let prior: Vec<String> = input
        .prior
        .iter()
        .map(|path| rel(path))
        .collect::<Result<_, _>>()?;
    let lookup = |live: Vec<String>, keys: &[UniqueKey]| -> Result<Vec<(usize, EntityId)>, String> {
        let runs = tessera_store::manifest::UniqueIndexRuns {
            attribute: declared.name.clone(),
            base: Vec::new(),
            live,
        };
        let index = UniqueIndex::open(&runs, kind, &input.prefix_dir, None)
            .map_err(|e| e.to_string())?;
        Ok(index
            .lookup(keys)
            .map_err(|e| e.to_string())?
            .into_iter()
            .map(|(i, entity)| (i, EntityId::new(u64::from(entity))))
            .filter(|(_, entity)| !deleted(*entity))
            .collect())
    };

    // This round's values against the earlier rounds' runs.
    if !prior.is_empty() {
        let mut entries: Vec<(UniqueKey, EntityId)> = Vec::new();
        for run in &written {
            tessera_store::unique::for_each_entry(kind, &run.path, |key, entity| {
                entries.push((key, EntityId::new(u64::from(entity))));
                Ok(())
            })
            .map_err(|e| e.to_string())?;
        }
        let keys: Vec<UniqueKey> = entries.iter().map(|(key, _)| *key).collect();
        for (i, holder) in lookup(prior.clone(), &keys)? {
            if holder != entries[i].1 {
                note(entries[i].0, &mut duplicates, &mut duplicate_keys);
            }
        }
    }

    // The buffered values, among themselves and against every run.
    let mut checked: FxHashMap<EntityId, UniqueKey> = FxHashMap::default();
    let mut by_key: FxHashMap<UniqueKey, EntityId> = FxHashMap::default();
    let mut texts: FxHashMap<UniqueKey, String> = FxHashMap::default();
    for (entity, scalars) in buffered_scalars(&generation.buffer) {
        if deleted(entity) {
            continue;
        }
        let Some(value) = scalars.get(input.at) else {
            continue;
        };
        let Some(key) = key_of(ty, value) else {
            continue;
        };
        if let WalScalar::Utf8(text) = value {
            texts.entry(key).or_insert_with(|| text.clone());
        }
        checked.insert(entity, key);
        if by_key.insert(key, entity).is_some_and(|other| other != entity) {
            note(key, &mut duplicates, &mut duplicate_keys);
        }
    }
    let buffered: Vec<(EntityId, UniqueKey)> = checked.iter().map(|(e, k)| (*e, *k)).collect();
    let keys: Vec<UniqueKey> = buffered.iter().map(|(_, key)| *key).collect();
    let every_run: Vec<String> = prior.iter().chain(&this_round).cloned().collect();
    if !keys.is_empty() && !every_run.is_empty() {
        for (i, holder) in lookup(every_run, &keys)? {
            if holder != buffered[i].0 {
                note(buffered[i].1, &mut duplicates, &mut duplicate_keys);
            }
        }
    }

    let examples = if duplicates == 0 {
        Vec::new()
    } else {
        describe_keys(generation, input.at, &input.wanted, &duplicate_keys, &texts)?
    };
    let mut runs = Vec::with_capacity(written.len());
    let paths: Vec<std::path::PathBuf> = written.iter().map(|run| run.path.clone()).collect();
    tessera_store::fsync_written(&paths).map_err(|e| e.to_string())?;
    for run in written {
        let digest = tessera_store::digest_of(&run.path).map_err(|e| e.to_string())?;
        runs.push((run, digest));
    }
    Ok(RoundOutput {
        runs,
        duplicates,
        examples,
        checked,
    })
}

/// Every buffered row's and fill's entity-scoped scalars, with the entity.
pub(crate) fn buffered_scalars(buffer: &IngestBuffer) -> Vec<(EntityId, &[WalScalar])> {
    buffer
        .iter()
        .map(|(entity, item)| (*entity, item.scalars.as_slice()))
        .chain(
            buffer
                .fills()
                .map(|(entity, fill)| (*entity, fill.scalars.as_slice())),
        )
        .collect()
}

/// The values of `keys` as a refusal names them. An integer key is its own value; a keyword key
/// is a hash, so its text is found among the buffered values or read again from the flushed
/// ones.
fn describe_keys(
    generation: &Generation,
    at: usize,
    wanted: &croaring::Bitmap,
    keys: &[UniqueKey],
    texts: &FxHashMap<UniqueKey, String>,
) -> Result<Vec<String>, String> {
    use tessera_store::unique::KeyKind;
    let declared = &generation.bundle.manifest.declared_scalars[at];
    let kind = KeyKind::of(declared.arrow_type);
    let mut found: FxHashMap<UniqueKey, String> = FxHashMap::default();
    let missing: Vec<UniqueKey> = keys
        .iter()
        .filter(|key| matches!(key, UniqueKey::Keyword(_)) && !texts.contains_key(key))
        .copied()
        .collect();
    if !missing.is_empty() {
        for_each_flushed_value(generation, at, wanted, &mut |_, value| {
            if let Some(key) = key_of(declared.arrow_type, &value) {
                if missing.contains(&key) && !found.contains_key(&key) {
                    found.insert(key, describe(&value));
                }
            }
            Ok(())
        })?;
    }
    Ok(keys
        .iter()
        .map(|key| match (*key, kind) {
            (UniqueKey::Int(k), Some(KeyKind::Unsigned)) => k.to_string(),
            (UniqueKey::Int(k), _) => tessera_store::key_index::signed_value(k).to_string(),
            (UniqueKey::Keyword(_), _) => texts
                .get(key)
                .map(|text| format!("'{text}'"))
                .or_else(|| found.get(key).cloned())
                .unwrap_or_else(|| "a value no longer held".to_string()),
        })
        .collect())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn live() -> UniqueLive {
        let mut live = UniqueLive::default();
        live.columns.insert("id".to_string(), Arc::default());
        live
    }

    /// An entry added after a handler's number is found by the re-check from the moment it is
    /// added, and still after a flush moves it into a run; the re-check is stale only once the
    /// entry has left the kept flushes.
    #[test]
    fn an_entry_added_after_a_check_is_found_until_it_leaves_the_kept_flushes() {
        let mut live = live();
        let seen = live.seq();
        let key = UniqueKey::unsigned(7);
        let entity = EntityId::new(3);
        live.add("id", key, entity);
        assert_eq!(live.added_since("id", &[key], seen), vec![(0, entity)]);
        live.flushed([("id", &[(key, entity)][..])]);
        assert!(live.lookup("id", &[key]).is_empty(), "a run holds it now");
        assert_eq!(live.added_since("id", &[key], seen), vec![(0, entity)]);
        assert!(!live.stale_since(seen));

        // Enough later flushes push it out, and the check made before it is then stale.
        let filler: Vec<(UniqueKey, EntityId)> = (0..RETIRED_MAX_ENTRIES as u64)
            .map(|i| (UniqueKey::unsigned(1_000 + i), EntityId::new(100 + i)))
            .collect();
        for (k, e) in &filler {
            live.add("id", *k, *e);
        }
        live.flushed([("id", &filler[..])]);
        assert!(live.stale_since(seen));
        assert!(!live.stale_since(live.seq()));
    }
}
