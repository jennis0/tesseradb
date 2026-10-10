//! Edited items: which entity holds an item whose number is not its entity.
//!
//! An item's number is the entity id it was first given, and its `mosaica_id` is the keyed
//! permutation of that number. An edit moves an item to a new entity and keeps its number, so an
//! item edited at least once needs a map from its number to the entity holding it and back. Items
//! never edited are in no map: their entity is their number.
//!
//! The map is two indexes in the run format of [`crate::key_index`] with four-byte keys:
//! `by_number` keys a number to the entities that have held it, and `by_entity` keys an entity to
//! its number. Each is a list of base runs with disjoint key ranges, written by a fold, and a list
//! of live runs written since, one per flush that wrote an edited item's own row and merged by a
//! coalesce. A fold drops the entries of the entities it removes. Entries are never removed
//! otherwise, so a caller drops deleted entities: after that an item has at most one entity.
//!
//! A segment whose rows hold an edited item carries [`EDITED_ROWS_FILE`] beside its columns: the
//! `(row, entity)` pairs, ascending by row, of the rows whose entity is not the one their
//! `mosaica_id` inverts to. Opening, merging and folding a segment read a row's entity from it.

use std::num::NonZeroU64;
use std::path::{Path, PathBuf};
use std::sync::{Arc, OnceLock};

use crate::error::{Result, StoreError};
use crate::key_index::{merge_runs, KeyRun, KeyRunWriter, WrittenRun};
use crate::manifest::{BaseKeyRun, EditedItemsRuns, KeyRuns};

/// The file a segment lists its rows' entities in where a row's entity is not its number.
pub const EDITED_ROWS_FILE: &str = "edited-rows.u32";

/// The most entries one run file holds before a writer starts the next.
const RUN_MAX_ENTRIES: NonZeroU64 = match NonZeroU64::new(1 << 26) {
    Some(n) => n,
    None => unreachable!(),
};

/// Which of the two indexes a run belongs to.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Direction {
    /// Keyed by number; each entry names an entity that has held it.
    ByNumber,
    /// Keyed by entity; each entry names the entity's number.
    ByEntity,
}

impl Direction {
    /// The direction's directory name.
    pub fn name(self) -> &'static str {
        match self {
            Direction::ByNumber => "by_number",
            Direction::ByEntity => "by_entity",
        }
    }
}

/// Where a partition's edited-item runs of one direction live, prefix-relative.
pub fn runs_dir_rel(partition: &str, direction: Direction) -> String {
    format!(
        "partitions/{partition}/entities/edited/{}",
        direction.name()
    )
}

/// The runs one writer finished for both directions.
#[derive(Debug, Default)]
pub struct WrittenEdited {
    pub by_number: Vec<WrittenRun<u32>>,
    pub by_entity: Vec<WrittenRun<u32>>,
}

impl WrittenEdited {
    /// Every run file written, both directions.
    pub fn paths(&self) -> impl Iterator<Item = &Path> {
        self.by_number
            .iter()
            .chain(&self.by_entity)
            .map(|run| run.path.as_path())
    }
}

/// Write `(number, entity)` pairs as runs of both directions named `<stem>-<n>.keys` under
/// `prefix_dir`. Writes no file for no pairs. The caller fsyncs.
pub fn write_edited_runs(
    prefix_dir: &Path,
    partition: &str,
    stem: &str,
    pairs: &[(u32, u32)],
) -> Result<WrittenEdited> {
    let mut by_number = pairs.to_vec();
    by_number.sort_unstable();
    by_number.dedup();
    let mut by_entity: Vec<(u32, u32)> = by_number.iter().map(|&(n, e)| (e, n)).collect();
    by_entity.sort_unstable();
    Ok(WrittenEdited {
        by_number: write_direction(prefix_dir, partition, Direction::ByNumber, stem, &by_number)?,
        by_entity: write_direction(prefix_dir, partition, Direction::ByEntity, stem, &by_entity)?,
    })
}

fn write_direction(
    prefix_dir: &Path,
    partition: &str,
    direction: Direction,
    stem: &str,
    sorted: &[(u32, u32)],
) -> Result<Vec<WrittenRun<u32>>> {
    if sorted.is_empty() {
        return Ok(Vec::new());
    }
    let dir = prefix_dir.join(runs_dir_rel(partition, direction));
    std::fs::create_dir_all(&dir).map_err(|source| StoreError::Io {
        path: dir.clone(),
        source,
    })?;
    let mut writer = KeyRunWriter::<u32>::create(&dir, stem, RUN_MAX_ENTRIES);
    for &(key, value) in sorted {
        writer.push(key, value)?;
    }
    writer.finish()
}

/// Merge one direction's runs at `inputs` into runs named `<stem>-<n>.keys` under `dir`,
/// dropping every entry of an entity `retired` holds.
pub fn merge_edited_runs(
    direction: Direction,
    inputs: &[PathBuf],
    retired: &croaring::Bitmap,
    dir: &Path,
    stem: &str,
) -> Result<Vec<WrittenRun<u32>>> {
    if inputs.is_empty() {
        return Ok(Vec::new());
    }
    std::fs::create_dir_all(dir).map_err(|source| StoreError::Io {
        path: dir.to_path_buf(),
        source,
    })?;
    match direction {
        Direction::ByNumber => merge_runs::<u32>(
            inputs,
            |_, entity| retired.contains(entity),
            dir,
            stem,
            RUN_MAX_ENTRIES,
        ),
        Direction::ByEntity => merge_runs::<u32>(
            inputs,
            |entity, _| retired.contains(entity),
            dir,
            stem,
            RUN_MAX_ENTRIES,
        ),
    }
}

/// A written run as a manifest's base run, its path made relative to `prefix_dir`.
pub fn as_base(prefix_dir: &Path, run: &WrittenRun<u32>) -> Result<BaseKeyRun> {
    Ok(BaseKeyRun {
        path: crate::unique::relative(prefix_dir, &run.path)?,
        first_key: format!("{:x}", run.min_key),
        last_key: format!("{:x}", run.max_key),
    })
}

/// One run, opened at its first lookup and shared with every later index naming the same file.
struct Slot {
    rel: String,
    path: PathBuf,
    run: OnceLock<KeyRun<u32>>,
}

impl Slot {
    fn run(&self) -> Result<&KeyRun<u32>> {
        if let Some(run) = self.run.get() {
            return Ok(run);
        }
        let opened = KeyRun::open(&self.path)?;
        let _ = self.run.set(opened);
        Ok(self.run.get().expect("set above"))
    }
}

/// One direction's runs: the base runs with their key ranges, and the live runs.
struct DirectionIndex {
    /// Ascending by range and disjoint.
    base: Vec<(u32, u32, Arc<Slot>)>,
    live: Vec<Arc<Slot>>,
}

impl DirectionIndex {
    fn open(
        runs: &KeyRuns,
        direction: Direction,
        prefix_dir: &Path,
        reuse: Option<&DirectionIndex>,
    ) -> Result<Self> {
        let held = |rel: &str| -> Arc<Slot> {
            reuse
                .and_then(|index| index.slots().find(|slot| slot.rel == rel))
                .cloned()
                .unwrap_or_else(|| {
                    Arc::new(Slot {
                        rel: rel.to_string(),
                        path: prefix_dir.join(rel),
                        run: OnceLock::new(),
                    })
                })
        };
        let malformed = |detail: String| StoreError::MalformedBundle { detail };
        let mut base = Vec::with_capacity(runs.base.len());
        for run in &runs.base {
            let parse = |hex: &str| {
                u32::from_str_radix(hex, 16).map_err(|_| {
                    malformed(format!(
                        "the edited items' {} base run {} has key bound '{hex}', which is not a \
                         32-bit hex number",
                        direction.name(),
                        run.path
                    ))
                })
            };
            let (lo, hi) = (parse(&run.first_key)?, parse(&run.last_key)?);
            if lo > hi {
                return Err(malformed(format!(
                    "the edited items' {} base run {} has its first key after its last",
                    direction.name(),
                    run.path
                )));
            }
            base.push((lo, hi, held(&run.path)));
        }
        if base.windows(2).any(|w| w[0].1 >= w[1].0) {
            return Err(malformed(format!(
                "the edited items' {} base runs overlap or are out of order; list them in \
                 ascending key order with disjoint ranges",
                direction.name()
            )));
        }
        Ok(DirectionIndex {
            base,
            live: runs.live.iter().map(|rel| held(rel)).collect(),
        })
    }

    fn slots(&self) -> impl Iterator<Item = &Arc<Slot>> {
        self.base.iter().map(|(_, _, slot)| slot).chain(&self.live)
    }

    /// The highest key any run holds, `None` for no runs.
    fn highest(&self) -> Result<Option<u32>> {
        let mut highest = self.base.last().map(|(_, hi, _)| *hi);
        for slot in &self.live {
            if let Some((_, hi)) = slot.run()?.key_range() {
                highest = highest.max(Some(hi));
            }
        }
        Ok(highest)
    }

    /// Every value stored under each of `keys`, as `(position in keys, value)`, ascending by
    /// position, each pair once.
    fn lookup(&self, keys: &[u32]) -> Result<Vec<(usize, u32)>> {
        let mut order: Vec<usize> = (0..keys.len()).collect();
        order.sort_by_key(|&i| keys[i]);
        let sorted: Vec<u32> = order.iter().map(|&i| keys[i]).collect();
        let mut out: Vec<(usize, u32)> = Vec::new();
        for slot in &self.live {
            for (i, value) in slot.run()?.lookup_sorted(&sorted)? {
                out.push((order[i], value));
            }
        }
        for (lo, hi, slot) in &self.base {
            let from = sorted.partition_point(|k| k < lo);
            let to = sorted.partition_point(|k| k <= hi);
            if from >= to {
                continue;
            }
            for (i, value) in slot.run()?.lookup_sorted(&sorted[from..to])? {
                out.push((order[from + i], value));
            }
        }
        out.sort_unstable();
        out.dedup();
        Ok(out)
    }
}

/// Both directions of one partition's edited-item runs. Reads no file until a lookup.
pub struct EditedIndex {
    by_number: DirectionIndex,
    by_entity: DirectionIndex,
}

impl EditedIndex {
    /// The runs `runs` names under `prefix_dir`, sharing each run `reuse` already holds by the
    /// same path.
    pub fn open(
        runs: &EditedItemsRuns,
        prefix_dir: &Path,
        reuse: Option<&EditedIndex>,
    ) -> Result<EditedIndex> {
        Ok(EditedIndex {
            by_number: DirectionIndex::open(
                &runs.by_number,
                Direction::ByNumber,
                prefix_dir,
                reuse.map(|r| &r.by_number),
            )?,
            by_entity: DirectionIndex::open(
                &runs.by_entity,
                Direction::ByEntity,
                prefix_dir,
                reuse.map(|r| &r.by_entity),
            )?,
        })
    }

    /// An index over no runs.
    pub fn empty() -> EditedIndex {
        EditedIndex {
            by_number: DirectionIndex {
                base: Vec::new(),
                live: Vec::new(),
            },
            by_entity: DirectionIndex {
                base: Vec::new(),
                live: Vec::new(),
            },
        }
    }

    /// Every entity that has held each of `numbers`, as `(position in numbers, entity)`.
    pub fn entities_of(&self, numbers: &[u32]) -> Result<Vec<(usize, u32)>> {
        self.by_number.lookup(numbers)
    }

    /// The number of each of `entities` that holds an edited item, as `(position, number)`.
    pub fn numbers_of(&self, entities: &[u32]) -> Result<Vec<(usize, u32)>> {
        self.by_entity.lookup(entities)
    }

    /// The highest entity any run gives a number, `None` where no run holds one: an entity above
    /// it holds no edited item's number in a run.
    pub fn highest_entity(&self) -> Result<Option<u32>> {
        self.by_entity.highest()
    }
}

/// Every `(key, value)` entry of one direction's runs, base and live, in no particular order.
pub fn entries(runs: &KeyRuns, prefix_dir: &Path) -> Result<Vec<(u32, u32)>> {
    let mut out = Vec::new();
    for rel in runs.base.iter().map(|run| &run.path).chain(&runs.live) {
        for entry in KeyRun::<u32>::open(&prefix_dir.join(rel))?.iter() {
            out.push(entry?);
        }
    }
    Ok(out)
}

impl Default for EditedIndex {
    fn default() -> Self {
        EditedIndex::empty()
    }
}

/// Write a segment's `(row, entity)` pairs, ascending by row, as [`EDITED_ROWS_FILE`] under
/// `seg_dir`. Writes nothing and answers `false` for no pairs.
pub fn write_edited_rows(seg_dir: &Path, rows: &[(u32, u32)]) -> Result<bool> {
    if rows.is_empty() {
        return Ok(false);
    }
    debug_assert!(rows.windows(2).all(|w| w[0].0 < w[1].0));
    let flat: Vec<u32> = rows
        .iter()
        .flat_map(|&(row, entity)| [row, entity])
        .collect();
    crate::flush::write_u32_array(&seg_dir.join(EDITED_ROWS_FILE), &flat)?;
    Ok(true)
}

/// A segment's [`EDITED_ROWS_FILE`], mapped: `(row, entity)` pairs strictly ascending by row.
#[derive(Debug)]
pub struct EditedRows {
    map: Option<memmap2::Mmap>,
}

impl EditedRows {
    /// No pairs.
    pub fn none() -> EditedRows {
        EditedRows { map: None }
    }

    /// The file under `seg_dir` where `listed` says a manifest names it, refused where it is
    /// missing, is not whole pairs or does not ascend by row; no pairs where it is not listed.
    pub fn open(seg_dir: &Path, listed: bool) -> Result<EditedRows> {
        if !listed {
            return Ok(EditedRows::none());
        }
        let path = seg_dir.join(EDITED_ROWS_FILE);
        let file = std::fs::File::open(&path).map_err(|source| StoreError::Io {
            path: path.clone(),
            source,
        })?;
        // SAFETY: a manifest names this file and a publisher never changes a file it has named;
        // see `MortonSlice::load` for the shared hazard of a concurrently truncated file.
        let map = unsafe { memmap2::Mmap::map(&file) }.map_err(|source| StoreError::Io {
            path: path.clone(),
            source,
        })?;
        if map.len() % 8 != 0 {
            return Err(StoreError::MalformedBundle {
                detail: format!(
                    "{} is {} bytes, not a whole number of (row, entity) pairs",
                    path.display(),
                    map.len()
                ),
            });
        }
        let rows = EditedRows { map: Some(map) };
        if rows
            .iter()
            .zip(rows.iter().skip(1))
            .any(|(a, b)| a.0 >= b.0)
        {
            return Err(StoreError::MalformedBundle {
                detail: format!("{} is not strictly ascending by row", path.display()),
            });
        }
        Ok(rows)
    }

    fn flat(&self) -> &[u32] {
        match &self.map {
            // SAFETY: the length is a checked multiple of 8 and a mapping is page-aligned.
            Some(map) => unsafe {
                std::slice::from_raw_parts(map.as_ptr() as *const u32, map.len() / 4)
            },
            None => &[],
        }
    }

    pub fn len(&self) -> usize {
        self.flat().len() / 2
    }

    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }

    /// Every pair, ascending by row.
    pub fn iter(&self) -> impl Iterator<Item = (u32, u32)> + '_ {
        self.flat().as_chunks::<2>().0.iter().map(|&[row, entity]| (row, entity))
    }

    /// The entity `row` lists, if it is one an edit moved.
    pub fn entity_of(&self, row: u32) -> Option<u32> {
        let pairs = self.flat();
        let (mut lo, mut hi) = (0usize, pairs.len() / 2);
        while lo < hi {
            let mid = (lo + hi) / 2;
            match pairs[2 * mid].cmp(&row) {
                std::cmp::Ordering::Less => lo = mid + 1,
                std::cmp::Ordering::Greater => hi = mid,
                std::cmp::Ordering::Equal => return Some(pairs[2 * mid + 1]),
            }
        }
        None
    }
}

/// Whether a file list names a segment's [`EDITED_ROWS_FILE`].
pub fn lists_edited_rows<'a>(files: impl IntoIterator<Item = &'a String>) -> bool {
    files
        .into_iter()
        .any(|rel| rel.rsplit('/').next() == Some(EDITED_ROWS_FILE))
}

/// Where the entity of each of a segment's rows is read.
#[derive(Debug, Clone)]
pub enum RowEntities {
    /// Every row's entity is the number its `mosaica_id` inverts to.
    Numbers,
    /// A flush or merge segment: the rows an edit moved are listed, and every other row's entity
    /// is its number.
    Listed(Arc<EditedRows>),
    /// A base segment: its view's row-to-entity file names every row's entity.
    Table(Arc<crate::row_entity::RowToEntity>),
}

impl RowEntities {
    /// The `(row, entity)` of every row an edit moved: every listed row, and every base row whose
    /// table names another entity than its `mosaica_id`'s number.
    pub fn moved<'a>(
        &'a self,
        mosaica_ids: &'a [u64],
        key: &'a mosaica_types::IdentityKey,
    ) -> Box<dyn Iterator<Item = (u32, u32)> + 'a> {
        match self {
            RowEntities::Numbers => Box::new(std::iter::empty()),
            RowEntities::Listed(rows) => Box::new(rows.iter()),
            RowEntities::Table(_) => {
                Box::new((0..mosaica_ids.len() as u32).filter_map(move |row| {
                    let entity = self.recorded(row)?;
                    let (_, number) =
                        key.invert(mosaica_types::MosaicaId::new(mosaica_ids[row as usize]));
                    (u64::from(entity) != number.raw()).then_some((row, entity))
                }))
            }
        }
    }

    /// The entity `row` is recorded under, where one is recorded: every base row with a table,
    /// and every row an edit moved in a flush or merge segment.
    pub fn recorded(&self, row: u32) -> Option<u32> {
        match self {
            RowEntities::Numbers => None,
            RowEntities::Listed(rows) => rows.entity_of(row),
            RowEntities::Table(table) => table
                .entity_of(mosaica_types::RowId::new(row))
                .map(|entity| entity.raw() as u32),
        }
    }

    /// The entity `row` belongs to. Refused where its `mosaica_id` inverts to another shard, or
    /// a base row lies past its table.
    pub fn entity_of(
        &self,
        row: u32,
        mosaica_id: u64,
        key: &mosaica_types::IdentityKey,
        shard_id: u32,
        seg_id: &str,
    ) -> Result<mosaica_types::EntityId> {
        let (shard, number) = key.invert(mosaica_types::MosaicaId::new(mosaica_id));
        if shard != shard_id {
            return Err(StoreError::MalformedBundle {
                detail: format!(
                    "segment '{seg_id}': row {row}'s mosaica_id inverts to shard {shard}, but the \
                     manifest declares shard {shard_id}"
                ),
            });
        }
        Ok(match self {
            RowEntities::Numbers => number,
            RowEntities::Listed(rows) => rows.entity_of(row).map_or(number, |entity| {
                mosaica_types::EntityId::new(u64::from(entity))
            }),
            RowEntities::Table(table) => table
                .entity_of(mosaica_types::RowId::new(row))
                .ok_or_else(|| StoreError::MalformedBundle {
                    detail: format!(
                        "segment '{seg_id}': row {row} lies past its view's row-to-entity file"
                    ),
                })?,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Both directions answer from the runs one write produced, and a merge drops the entries of
    /// the entities it retires in each direction.
    #[test]
    fn both_directions_answer_and_a_merge_drops_retired_entities() {
        let tmp = tempfile::tempdir().unwrap();
        let prefix = tmp.path();
        let written = write_edited_runs(prefix, "p", "live", &[(3, 40), (3, 41), (7, 42)]).unwrap();
        let listed = |runs: &[WrittenRun<u32>]| KeyRuns {
            base: Vec::new(),
            live: runs
                .iter()
                .map(|r| crate::unique::relative(prefix, &r.path).unwrap())
                .collect(),
        };
        let runs = EditedItemsRuns {
            by_number: listed(&written.by_number),
            by_entity: listed(&written.by_entity),
        };
        let index = EditedIndex::open(&runs, prefix, None).unwrap();
        assert_eq!(
            index.entities_of(&[7, 3, 9]).unwrap(),
            vec![(0, 42), (1, 40), (1, 41)]
        );
        assert_eq!(index.numbers_of(&[41, 5]).unwrap(), vec![(0, 3)]);

        let retired = croaring::Bitmap::of(&[40]);
        let paths =
            |runs: &[WrittenRun<u32>]| runs.iter().map(|r| r.path.clone()).collect::<Vec<_>>();
        let dir = |direction| prefix.join(runs_dir_rel("p", direction));
        let by_number = merge_edited_runs(
            Direction::ByNumber,
            &paths(&written.by_number),
            &retired,
            &dir(Direction::ByNumber),
            "base",
        )
        .unwrap();
        let by_entity = merge_edited_runs(
            Direction::ByEntity,
            &paths(&written.by_entity),
            &retired,
            &dir(Direction::ByEntity),
            "base",
        )
        .unwrap();
        let based = |runs: &[WrittenRun<u32>]| KeyRuns {
            base: runs.iter().map(|r| as_base(prefix, r).unwrap()).collect(),
            live: Vec::new(),
        };
        let folded = EditedIndex::open(
            &EditedItemsRuns {
                by_number: based(&by_number),
                by_entity: based(&by_entity),
            },
            prefix,
            None,
        )
        .unwrap();
        assert_eq!(folded.entities_of(&[3]).unwrap(), vec![(0, 41)]);
        assert_eq!(folded.numbers_of(&[40, 41]).unwrap(), vec![(1, 3)]);
    }

    #[test]
    fn a_segments_pairs_name_the_entity_of_their_rows_and_nothing_else() {
        let tmp = tempfile::tempdir().unwrap();
        assert!(EditedRows::open(tmp.path(), false).unwrap().is_empty());
        assert!(
            EditedRows::open(tmp.path(), true).is_err(),
            "a listed file that is missing is refused"
        );
        assert!(!write_edited_rows(tmp.path(), &[]).unwrap());
        assert!(write_edited_rows(tmp.path(), &[(1, 90), (4, 91)]).unwrap());
        let rows = EditedRows::open(tmp.path(), true).unwrap();
        assert_eq!(rows.iter().collect::<Vec<_>>(), vec![(1, 90), (4, 91)]);
        let key = mosaica_types::IdentityKey::from_hex("0123456789abcdef0123456789abcdef").unwrap();
        let tid = |n: u64| {
            key.forward(0, mosaica_types::EntityId::new(n))
                .unwrap()
                .raw()
        };
        let rows = RowEntities::Listed(Arc::new(rows));
        assert_eq!(
            rows.entity_of(4, tid(5), &key, 0, "s").unwrap(),
            mosaica_types::EntityId::new(91)
        );
        assert_eq!(
            rows.entity_of(2, tid(5), &key, 0, "s").unwrap(),
            mosaica_types::EntityId::new(5)
        );
    }
}
