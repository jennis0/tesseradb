//! Unique fields at a running service: the index entries of values no run holds yet, and the one
//! lookup every reader of a unique column goes through.
//!
//! A unique column's values reach its index in two steps. A row accepted into the ingest buffer
//! adds a live entry here at its window's close; the flush that writes the row
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
use tessera_store::unique::{key_of, key_text, value_text, UniqueKey, DUPLICATE_EXAMPLES};
use tessera_types::EntityId;

use crate::Generation;

/// One entity holding a key, and the sequence number the entry was added under.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
struct Entry {
    key: UniqueKey,
    entity: EntityId,
    seq: u64,
}

/// Entries sorted by key, then entity.
type Sorted = Arc<[Entry]>;

/// The entries that start at `key` in `run`.
fn entries_of<'a>(run: &'a [Entry], key: &UniqueKey) -> impl Iterator<Item = &'a Entry> {
    let key = *key;
    let from = run.partition_point(|e| e.key < key);
    run[from..].iter().take_while(move |e| e.key == key)
}

/// One column's live entries, as sorted runs, oldest first. A run is merged into the one before
/// it while it is at least half that one's size, so a column holds a logarithmic number of runs,
/// an entry is copied a logarithmic number of times, and a generation's copy shares every run.
#[derive(Debug, Clone, Default)]
struct Column {
    runs: Vec<Sorted>,
}

impl Column {
    fn push(&mut self, mut added: Vec<Entry>) {
        if added.is_empty() {
            return;
        }
        added.sort_unstable();
        let mut run = added;
        while let Some(last) = self.runs.last() {
            if last.len() > 2 * run.len() {
                break;
            }
            let last = self.runs.pop().expect("a last run");
            let mut merged = Vec::with_capacity(last.len() + run.len());
            let (mut i, mut j) = (0, 0);
            while i < last.len() && j < run.len() {
                if last[i] <= run[j] {
                    merged.push(last[i]);
                    i += 1;
                } else {
                    merged.push(run[j]);
                    j += 1;
                }
            }
            merged.extend_from_slice(&last[i..]);
            merged.extend_from_slice(&run[j..]);
            run = merged;
        }
        self.runs.push(run.into());
    }

    /// Remove every entry `gone` answers for, rewriting only the runs that hold one, and answer
    /// what was removed.
    fn remove_where(&mut self, gone: impl Fn(&Entry) -> bool) -> Vec<Entry> {
        let mut removed = Vec::new();
        for run in &mut self.runs {
            if !run.iter().any(&gone) {
                continue;
            }
            let (out, kept): (Vec<Entry>, Vec<Entry>) = run.iter().partition(|e| gone(e));
            removed.extend(out);
            *run = kept.into();
        }
        self.runs.retain(|run| !run.is_empty());
        removed
    }

    fn entries<'a>(&'a self, key: &'a UniqueKey) -> impl Iterator<Item = &'a Entry> {
        self.runs.iter().flat_map(move |run| entries_of(run, key))
    }
}

/// The entries one flush moved into runs, by attribute.
type Retired = BTreeMap<String, Sorted>;

/// How many moved entries are kept for the executor's re-check, across the flushes that moved
/// them. The newest flush's are kept whatever their number.
const RETIRED_MAX_ENTRIES: usize = 1 << 20;

/// The index entries no run holds yet, per unique column. Cloning one copies a few pointers per
/// column, since every generation shares the entries.
#[derive(Debug, Clone, Default)]
pub(crate) struct UniqueLive {
    /// Keyed by attribute name.
    columns: BTreeMap<String, Column>,
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
    /// The live entries of every unique column `manifest` declares, from the rows `buffer` holds:
    /// what a restart or a declaration starts from.
    pub(crate) fn derive(manifest: &Manifest, buffer: &IngestBuffer) -> UniqueLive {
        let mut live = UniqueLive::default();
        for declared in manifest.declared_scalars.iter().filter(|d| d.unique) {
            live.columns.insert(declared.name.clone(), Column::default());
        }
        live.add(&manifest.declared_scalars, buffered_scalars(buffer));
        live
    }

    /// The sequence number of the last entry added: what a handler checked a batch at.
    pub(crate) fn seq(&self) -> u64 {
        self.seq
    }

    /// Whether an entry added after `seq` may be missing from here, or the unique columns have
    /// changed since `seq`, so a batch checked at `seq` cannot be re-checked from memory.
    pub(crate) fn stale_since(&self, seq: u64) -> bool {
        self.flushed_through > seq || self.columns_changed_since(seq)
    }

    /// Whether the unique columns have changed since `seq`.
    pub(crate) fn columns_changed_since(&self, seq: u64) -> bool {
        self.columns_changed_at > seq
    }

    /// Whether no column is unique, so nothing is ever added.
    pub(crate) fn is_empty_schema(&self) -> bool {
        self.columns.is_empty()
    }

    /// Add the entries of each entity's entity-scoped scalars, for every column held here.
    pub(crate) fn add<'a>(
        &mut self,
        declared: &[DeclaredScalar],
        rows: impl IntoIterator<Item = (EntityId, &'a [WalScalar])>,
    ) {
        let columns: Vec<(usize, &DeclaredScalar)> = declared
            .iter()
            .enumerate()
            .filter(|(_, d)| self.columns.contains_key(&d.name))
            .collect();
        if columns.is_empty() {
            return;
        }
        let mut added: Vec<Vec<Entry>> = vec![Vec::new(); columns.len()];
        for (entity, scalars) in rows {
            for ((at, d), into) in columns.iter().zip(&mut added) {
                if let Some(key) = scalars.get(*at).and_then(|v| key_of(d.arrow_type, v)) {
                    self.seq += 1;
                    into.push(Entry {
                        key,
                        entity,
                        seq: self.seq,
                    });
                }
            }
        }
        for ((_, d), entries) in columns.into_iter().zip(added) {
            if let Some(column) = self.columns.get_mut(&d.name) {
                column.push(entries);
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
            let mut written: Vec<(UniqueKey, EntityId)> = entries_written.to_vec();
            written.sort_unstable();
            let mut out =
                column.remove_where(|e| written.binary_search(&(e.key, e.entity)).is_ok());
            if out.is_empty() {
                continue;
            }
            count += out.len();
            out.sort_unstable();
            moved.insert(attribute.to_string(), out.into());
        }
        if count == 0 {
            return;
        }
        self.retired.push_back((Arc::new(moved), count));
        let mut held: usize = self.retired.iter().map(|(_, n)| n).sum();
        while held > RETIRED_MAX_ENTRIES && self.retired.len() > 1 {
            let (oldest, n) = self.retired.pop_front().expect("more than one held");
            held -= n;
            for entry in oldest.values().flat_map(|run| run.iter()) {
                self.flushed_through = self.flushed_through.max(entry.seq);
            }
        }
    }

    /// Remove every entry of the deleted entities. A deleted entity names nothing, so this changes
    /// no answer; it only returns the memory.
    pub(crate) fn remove_entities(&mut self, deleted: &croaring::Bitmap) {
        if deleted.is_empty() {
            return;
        }
        let gone = |e: &Entry| u32::try_from(e.entity.raw()).is_ok_and(|id| deleted.contains(id));
        for column in self.columns.values_mut() {
            column.remove_where(gone);
        }
    }

    /// Make `attribute` unique, with the entries of every row `buffer` holds, or stop it being
    /// unique.
    pub(crate) fn set_unique(
        &mut self,
        manifest: &Manifest,
        buffer: &IngestBuffer,
        attribute: &str,
        unique: bool,
    ) {
        if unique {
            let mut only = UniqueLive {
                seq: self.seq,
                ..UniqueLive::default()
            };
            only.columns.insert(attribute.to_string(), Column::default());
            only.add(&manifest.declared_scalars, buffered_scalars(buffer));
            self.seq = only.seq;
            self.columns.extend(only.columns);
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
        self.added_after(attribute, keys, 0, false)
    }

    /// [`Self::lookup`] over the entries added after `after`, the live ones and the kept ones a
    /// flush has moved into runs: the executor's re-check.
    pub(crate) fn added_since(
        &self,
        attribute: &str,
        keys: &[UniqueKey],
        after: u64,
    ) -> Vec<(usize, EntityId)> {
        self.added_after(attribute, keys, after, true)
    }

    fn added_after(
        &self,
        attribute: &str,
        keys: &[UniqueKey],
        after: u64,
        retired: bool,
    ) -> Vec<(usize, EntityId)> {
        let mut out = Vec::new();
        let column = self.columns.get(attribute);
        for (i, key) in keys.iter().enumerate() {
            let live = column.into_iter().flat_map(|c| c.entries(key));
            let kept = self
                .retired
                .iter()
                .filter(|_| retired)
                .filter_map(|(moved, _)| moved.get(attribute))
                .flat_map(|run| entries_of(run, key));
            out.extend(
                live.chain(kept)
                    .filter(|e| e.seq > after)
                    .map(|e| (i, e.entity)),
            );
        }
        out
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

/// An item's `tessera_id` as a refusal names it.
pub(crate) fn tessera_id_text(
    engine: &crate::Engine,
    generation: &Generation,
    entity: EntityId,
) -> String {
    engine
        .tessera_id_in(generation, entity)
        .map(|id| id.raw().to_string())
        .unwrap_or_else(|_| "unknown".to_string())
}

/// Whether a value in `keys`, each as `(declared position, key widened)`, may have been given a
/// holder since the live entries' sequence number was `seq`: the executor's re-check of a batch
/// its handler resolved at `seq`, from memory. Where an entry added since has already left
/// memory the answer is yes, and so it is for a batch that `creates` items once the unique
/// columns have changed, since its keys were taken under the old ones.
pub(crate) fn moved_since(
    generation: &Generation,
    keys: &[(u16, u128)],
    seq: u64,
    creates: bool,
) -> bool {
    use tessera_store::unique::KeyKind;
    let live = &generation.unique_live;
    if creates && live.columns_changed_since(seq) {
        return true;
    }
    if keys.is_empty() {
        return false;
    }
    if live.stale_since(seq) {
        return true;
    }
    let declared = &generation.bundle.manifest.declared_scalars;
    keys.iter().any(|(field, key)| {
        let Some(d) = declared.get(usize::from(*field)).filter(|d| d.unique) else {
            return true;
        };
        let Some(kind) = KeyKind::of(d.arrow_type) else {
            return true;
        };
        live.added_since(&d.name, &[UniqueKey::of_widened(kind, *key)], seq)
            .into_iter()
            .any(|(_, holder)| !generation.overlay.is_deleted(holder))
    })
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
    /// Keys more than one live or suppressed item holds, and up to
    /// [`DUPLICATE_EXAMPLES`] of their values.
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
    for_each_flushed_value(
        generation,
        input.at,
        &input.wanted,
        &mut |entity, value| match key_of(ty, &value) {
            Some(key) => spill.push(key, entity).map_err(|e| e.to_string()),
            None => Ok(()),
        },
    )?;
    let mut duplicate_keys: Vec<UniqueKey> = Vec::new();
    let mut duplicates = 0u64;
    let written = spill
        .finish(&input.out_dir, &input.stem, |d| {
            duplicates += 1;
            if duplicate_keys.len() < DUPLICATE_EXAMPLES {
                duplicate_keys.push(d.key);
            }
        })
        .map_err(|e| e.to_string())?;
    let note = |key: UniqueKey, duplicates: &mut u64, keys: &mut Vec<UniqueKey>| {
        if !keys.contains(&key) {
            *duplicates += 1;
            if keys.len() < DUPLICATE_EXAMPLES {
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
    let lookup =
        |live: Vec<String>, keys: &[UniqueKey]| -> Result<Vec<(usize, EntityId)>, String> {
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

/// Every buffered item's entity-scoped scalars, with the entity.
pub(crate) fn buffered_scalars(buffer: &IngestBuffer) -> Vec<(EntityId, &[WalScalar])> {
    buffer
        .iter()
        .map(|(entity, item)| (*entity, item.scalars.as_slice()))
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
                    found.insert(key, value_text(&value));
                }
            }
            Ok(())
        })?;
    }
    Ok(keys
        .iter()
        .map(|key| {
            kind.and_then(|kind| key_text(*key, kind))
                .or_else(|| {
                    texts
                        .get(key)
                        .map(|text| value_text(&WalScalar::Utf8(text.clone())))
                })
                .or_else(|| found.get(key).cloned())
                .unwrap_or_else(|| "a value no longer held".to_string())
        })
        .collect())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn live() -> UniqueLive {
        let mut live = UniqueLive::default();
        live.columns.insert("id".to_string(), Column::default());
        live
    }

    fn add(live: &mut UniqueLive, entries: &[(UniqueKey, EntityId)]) {
        let declared = [DeclaredScalar {
            name: "id".to_string(),
            arrow_type: tessera_spatial::tiler::ScalarType::U64,
            vocabulary: None,
            analyser: None,
            index: false,
            render: false,
            unique: true,
        }];
        let scalars: Vec<[WalScalar; 1]> = entries
            .iter()
            .map(|(key, _)| match key {
                UniqueKey::Int(k) => [WalScalar::U64(*k)],
                UniqueKey::Keyword(_) => unreachable!("an integer column"),
            })
            .collect();
        live.add(
            &declared,
            entries.iter().zip(&scalars).map(|((_, e), s)| (*e, &s[..])),
        );
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
        add(&mut live, &[(key, entity)]);
        assert_eq!(live.added_since("id", &[key], seen), vec![(0, entity)]);
        live.flushed([("id", &[(key, entity)][..])]);
        assert!(live.lookup("id", &[key]).is_empty(), "a run holds it now");
        assert_eq!(live.added_since("id", &[key], seen), vec![(0, entity)]);
        assert!(!live.stale_since(seen));

        // Enough later flushes push it out, and the check made before it is then stale.
        let filler: Vec<(UniqueKey, EntityId)> = (0..RETIRED_MAX_ENTRIES as u64)
            .map(|i| (UniqueKey::unsigned(1_000 + i), EntityId::new(100 + i)))
            .collect();
        add(&mut live, &filler);
        live.flushed([("id", &filler[..])]);
        assert!(live.stale_since(seen));
        assert!(!live.stale_since(live.seq()));
    }

    /// Entries added a window at a time are all found, in a logarithmic number of runs, and a
    /// flush or a deletion removes exactly its own.
    #[test]
    fn windows_of_entries_stay_in_few_runs_and_are_all_found() {
        let mut live = live();
        let entries: Vec<(UniqueKey, EntityId)> = (0..10_000u64)
            .map(|i| (UniqueKey::unsigned(i * 7_919 % 10_007), EntityId::new(i)))
            .collect();
        for window in entries.chunks(10) {
            add(&mut live, window);
        }
        let runs = live.columns["id"].runs.len();
        assert!(runs <= 2 * 14, "{runs} runs for 1,000 windows");
        let keys: Vec<UniqueKey> = entries.iter().map(|(k, _)| *k).collect();
        let found = live.lookup("id", &keys);
        assert_eq!(found.len(), entries.len());
        assert!(found.iter().all(|(i, e)| entries[*i].1 == *e));

        live.flushed([("id", &entries[..5_000])]);
        let deleted: croaring::Bitmap = (5_000u32..6_000).collect();
        live.remove_entities(&deleted);
        let left: Vec<EntityId> = live.lookup("id", &keys).into_iter().map(|(_, e)| e).collect();
        let expected: Vec<EntityId> = (6_000..10_000).map(EntityId::new).collect();
        let mut left = left;
        left.sort_unstable();
        assert_eq!(left, expected);
    }
}
