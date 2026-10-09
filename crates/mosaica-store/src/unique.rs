//! Unique fields: which columns may be declared unique, the key a value becomes, and the index over
//! one column's key runs.
//!
//! A unique column's index maps each value's key to the entities holding it, in the run format of
//! [`crate::key_index`]. Integer and timestamp values are 8-byte keys, a signed value mapped so
//! keys order as values do; a keyword value is the 16-byte XXH3-128 hash of its bytes. A null holds
//! no entry, so nulls never match and never collide.
//!
//! One column's index is a list of base runs with disjoint key ranges, written by the build and
//! the fold, and a list of live runs written since: one per flush, merged by a coalesce, and one
//! set per declaration made at a running service. A lookup gathers every entity stored under a
//! key in every live run and in the one base run whose range holds it. Entries are never removed
//! outside a fold, so the caller drops deleted entities; after that at most one entity remains
//! for any key.
//!
//! A run is opened at its first lookup, never at open: a base run's key range comes from the
//! manifest, so a lookup touches only the runs its keys fall in, and a page is checked the first
//! time it is read.

use std::collections::BTreeMap;
use std::num::NonZeroU64;
use std::path::{Path, PathBuf};
use std::sync::{Arc, OnceLock};

use mosaica_spatial::tiler::{ScalarType, ScalarValue};

use crate::error::{Result, StoreError};
use crate::key_index::{
    keyword_key, merge_runs, signed_key, unsigned_key, DuplicateKey, KeyRun, KeyRunWriter,
    KeySpill, WrittenRun,
};
use crate::manifest::{BaseKeyRun, DeclaredScalar, Manifest, SegmentsManifest, UniqueIndexRuns};

/// The most entries one run file holds before a writer starts the next, past the entities of its
/// last key. At most about 12 bytes an entry, an integer run is at most about 770 MiB.
pub const RUN_MAX_ENTRIES: NonZeroU64 = match NonZeroU64::new(1 << 26) {
    Some(n) => n,
    None => unreachable!(),
};

/// Where a partition's unique indexes live, prefix-relative: one directory per column.
pub fn index_dir_rel(partition: &str, attribute: &str) -> String {
    format!("partitions/{partition}/entities/unique/{attribute}")
}

/// Whether a column of this type may be declared unique: keyword, every integer width, and
/// timestamps. A float has no exact equality to index, a boolean two values, a category codes
/// minted by the server, and text no single value.
pub fn allows_unique(ty: ScalarType) -> bool {
    use ScalarType as T;
    matches!(
        ty,
        T::Keyword
            | T::U8
            | T::U16
            | T::U32
            | T::U64
            | T::I8
            | T::I16
            | T::I32
            | T::I64
            | T::TimestampUs
    )
}

/// One value's key, at its column's width.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum UniqueKey {
    /// An integer or timestamp value's key: [`unsigned_key`] or [`signed_key`].
    Int(u64),
    /// A keyword value's key: [`keyword_key`].
    Keyword(u128),
}

impl UniqueKey {
    /// The key of an unsigned integer value.
    pub fn unsigned(value: u64) -> UniqueKey {
        UniqueKey::Int(unsigned_key(value))
    }

    /// The key of a signed integer or timestamp value.
    pub fn signed(value: i64) -> UniqueKey {
        UniqueKey::Int(signed_key(value))
    }

    /// The key of a keyword value.
    pub fn keyword(value: &str) -> UniqueKey {
        UniqueKey::Keyword(keyword_key(value))
    }

    /// The key [`Self::widen`] made `widened` from, for a column whose keys are of `kind`.
    pub fn of_widened(kind: KeyKind, widened: u128) -> UniqueKey {
        match kind {
            KeyKind::Keyword => UniqueKey::Keyword(widened),
            KeyKind::Unsigned | KeyKind::Signed => UniqueKey::Int(widened as u64),
        }
    }

    /// The key zero-extended, as a manifest records a base run's range.
    pub fn widen(self) -> u128 {
        match self {
            UniqueKey::Int(k) => k as u128,
            UniqueKey::Keyword(k) => k,
        }
    }
}

/// Whether a column's keys are integers or keyword hashes, from its declared type.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum KeyKind {
    Unsigned,
    Signed,
    Keyword,
}

impl KeyKind {
    /// `None` where [`allows_unique`] refuses the type.
    pub fn of(ty: ScalarType) -> Option<KeyKind> {
        use ScalarType as T;
        Some(match ty {
            T::U8 | T::U16 | T::U32 | T::U64 => KeyKind::Unsigned,
            T::I8 | T::I16 | T::I32 | T::I64 | T::TimestampUs => KeyKind::Signed,
            T::Keyword => KeyKind::Keyword,
            _ => return None,
        })
    }

    fn is_keyword(self) -> bool {
        self == KeyKind::Keyword
    }
}

/// The key a unique column of type `ty` holds `value` under, or `None` for a null or a value of
/// another shape. An integer value is widened to 64 bits before its key is taken, so one value
/// has one key at every width.
pub fn key_of(ty: ScalarType, value: &ScalarValue) -> Option<UniqueKey> {
    use ScalarValue as V;
    let kind = KeyKind::of(ty)?;
    Some(match (kind, value) {
        (KeyKind::Keyword, V::Utf8(s)) => UniqueKey::keyword(s),
        (KeyKind::Unsigned, V::U8(v)) => UniqueKey::unsigned(u64::from(*v)),
        (KeyKind::Unsigned, V::U16(v)) => UniqueKey::unsigned(u64::from(*v)),
        (KeyKind::Unsigned, V::U32(v)) => UniqueKey::unsigned(u64::from(*v)),
        (KeyKind::Unsigned, V::U64(v)) => UniqueKey::unsigned(*v),
        (KeyKind::Signed, V::I8(v)) => UniqueKey::signed(i64::from(*v)),
        (KeyKind::Signed, V::I16(v)) => UniqueKey::signed(i64::from(*v)),
        (KeyKind::Signed, V::I32(v)) => UniqueKey::signed(i64::from(*v)),
        (KeyKind::Signed, V::I64(v)) | (KeyKind::Signed, V::TimestampUs(v)) => {
            UniqueKey::signed(*v)
        }
        _ => return None,
    })
}

/// The key of an exact integer for a unique integer or timestamp column, or `None` where the
/// column's type cannot hold the integer, which then matches nothing.
pub fn key_of_integer(ty: ScalarType, value: i128) -> Option<UniqueKey> {
    use ScalarType as T;
    let fits = match ty {
        T::U8 => u8::try_from(value).is_ok(),
        T::U16 => u16::try_from(value).is_ok(),
        T::U32 => u32::try_from(value).is_ok(),
        T::U64 => u64::try_from(value).is_ok(),
        T::I8 => i8::try_from(value).is_ok(),
        T::I16 => i16::try_from(value).is_ok(),
        T::I32 => i32::try_from(value).is_ok(),
        T::I64 | T::TimestampUs => i64::try_from(value).is_ok(),
        _ => false,
    };
    if !fits {
        return None;
    }
    match KeyKind::of(ty)? {
        KeyKind::Unsigned => Some(UniqueKey::unsigned(value as u64)),
        KeyKind::Signed => Some(UniqueKey::signed(value as i64)),
        KeyKind::Keyword => None,
    }
}

/// One run file a unique index writer finished.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WrittenUniqueRun {
    pub path: PathBuf,
    pub entries: u64,
    pub first: UniqueKey,
    pub last: UniqueKey,
}

impl WrittenUniqueRun {
    fn of_int(run: WrittenRun<u64>) -> Self {
        WrittenUniqueRun {
            path: run.path,
            entries: run.entries,
            first: UniqueKey::Int(run.min_key),
            last: UniqueKey::Int(run.max_key),
        }
    }

    fn of_keyword(run: WrittenRun<u128>) -> Self {
        WrittenUniqueRun {
            path: run.path,
            entries: run.entries,
            first: UniqueKey::Keyword(run.min_key),
            last: UniqueKey::Keyword(run.max_key),
        }
    }

    /// This run as a manifest's base run, its path made relative to `prefix_dir`.
    pub fn as_base(&self, prefix_dir: &Path) -> Result<BaseKeyRun> {
        Ok(BaseKeyRun {
            path: relative(prefix_dir, &self.path)?,
            first_key: format!("{:x}", self.first.widen()),
            last_key: format!("{:x}", self.last.widen()),
        })
    }
}

/// `path` relative to `prefix_dir`, with forward slashes, as a manifest names a file.
pub fn relative(prefix_dir: &Path, path: &Path) -> Result<String> {
    path.strip_prefix(prefix_dir)
        .ok()
        .and_then(|p| p.to_str())
        .map(|p| p.replace('\\', "/"))
        .ok_or_else(|| StoreError::MalformedBundle {
            detail: format!(
                "unique index run {} is not under the prefix {}",
                path.display(),
                prefix_dir.display()
            ),
        })
}

/// A key held by more than one entity, as a spill found it: the key, its two smallest entities
/// and how many entities hold it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct DuplicateUniqueKey {
    pub key: UniqueKey,
    pub first: [u32; 2],
    pub entities: u64,
}

/// Entries in any order, sorted through a scratch directory under a memory budget and written as
/// base runs with disjoint key ranges ([`KeySpill`]).
pub enum UniqueSpill {
    Int(KeySpill<u64>),
    Keyword(KeySpill<u128>),
}

impl UniqueSpill {
    pub fn create(kind: KeyKind, scratch: &Path, memory_budget: usize) -> Result<Self> {
        Ok(match kind.is_keyword() {
            true => UniqueSpill::Keyword(KeySpill::create(scratch, memory_budget)?),
            false => UniqueSpill::Int(KeySpill::create(scratch, memory_budget)?),
        })
    }

    pub fn push(&mut self, key: UniqueKey, entity: u32) -> Result<()> {
        match (self, key) {
            (UniqueSpill::Int(spill), UniqueKey::Int(k)) => spill.push(k, entity),
            (UniqueSpill::Keyword(spill), UniqueKey::Keyword(k)) => spill.push(k, entity),
            _ => Err(width_mismatch()),
        }
    }

    /// Write every entry as runs named `<stem>-<n>.keys` under `out_dir`, calling `on_duplicate`
    /// for each key more than one entity holds.
    pub fn finish(
        self,
        out_dir: &Path,
        stem: &str,
        mut on_duplicate: impl FnMut(DuplicateUniqueKey),
    ) -> Result<Vec<WrittenUniqueRun>> {
        Ok(match self {
            UniqueSpill::Int(spill) => spill
                .finish(out_dir, stem, RUN_MAX_ENTRIES, |d: DuplicateKey<u64>| {
                    on_duplicate(DuplicateUniqueKey {
                        key: UniqueKey::Int(d.key),
                        first: d.first,
                        entities: d.entities,
                    })
                })?
                .into_iter()
                .map(WrittenUniqueRun::of_int)
                .collect(),
            UniqueSpill::Keyword(spill) => spill
                .finish(out_dir, stem, RUN_MAX_ENTRIES, |d: DuplicateKey<u128>| {
                    on_duplicate(DuplicateUniqueKey {
                        key: UniqueKey::Keyword(d.key),
                        first: d.first,
                        entities: d.entities,
                    })
                })?
                .into_iter()
                .map(WrittenUniqueRun::of_keyword)
                .collect(),
        })
    }
}

fn width_mismatch() -> StoreError {
    StoreError::MalformedBundle {
        detail: "a unique index was given a key of the other width; an integer column's keys are \
                 integers and a keyword column's are hashes"
            .to_string(),
    }
}

/// Write entries already sorted by `(key, entity)` as runs named `<stem>-<n>.keys` under
/// `out_dir`. Writes no file for no entries. The caller fsyncs.
pub fn write_sorted_runs(
    kind: KeyKind,
    out_dir: &Path,
    stem: &str,
    entries: &[(UniqueKey, u32)],
) -> Result<Vec<WrittenUniqueRun>> {
    std::fs::create_dir_all(out_dir).map_err(|source| StoreError::Io {
        path: out_dir.to_path_buf(),
        source,
    })?;
    if kind.is_keyword() {
        let mut writer = KeyRunWriter::<u128>::create(out_dir, stem, RUN_MAX_ENTRIES);
        for &(key, entity) in entries {
            let UniqueKey::Keyword(k) = key else {
                return Err(width_mismatch());
            };
            writer.push(k, entity)?;
        }
        Ok(writer
            .finish()?
            .into_iter()
            .map(WrittenUniqueRun::of_keyword)
            .collect())
    } else {
        let mut writer = KeyRunWriter::<u64>::create(out_dir, stem, RUN_MAX_ENTRIES);
        for &(key, entity) in entries {
            let UniqueKey::Int(k) = key else {
                return Err(width_mismatch());
            };
            writer.push(k, entity)?;
        }
        Ok(writer
            .finish()?
            .into_iter()
            .map(WrittenUniqueRun::of_int)
            .collect())
    }
}

/// Merge the runs at `inputs` into runs named `<stem>-<n>.keys` under `out_dir`, dropping the
/// entries of every entity `retired` holds and keeping every other entry once. The output has
/// disjoint key ranges whatever the inputs' were.
pub fn merge_unique_runs(
    inputs: &[PathBuf],
    retired: &croaring::Bitmap,
    out_dir: &Path,
    stem: &str,
) -> Result<Vec<WrittenUniqueRun>> {
    let Some(first) = inputs.first() else {
        return Ok(Vec::new());
    };
    let keyword = crate::key_index::run_key_width(first)? == 16;
    std::fs::create_dir_all(out_dir).map_err(|source| StoreError::Io {
        path: out_dir.to_path_buf(),
        source,
    })?;
    Ok(match keyword {
        true => merge_runs::<u128>(
            inputs,
            |_, entity| retired.contains(entity),
            out_dir,
            stem,
            RUN_MAX_ENTRIES,
        )?
        .into_iter()
        .map(WrittenUniqueRun::of_keyword)
        .collect(),
        false => merge_runs::<u64>(
            inputs,
            |_, entity| retired.contains(entity),
            out_dir,
            stem,
            RUN_MAX_ENTRIES,
        )?
        .into_iter()
        .map(WrittenUniqueRun::of_int)
        .collect(),
    })
}

/// How a unique index's live entries compare with a column's values, as
/// [`compare_unique_runs`] finds them.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct IndexComparison {
    /// The index's entries less the retired entities'.
    pub index_entries: u64,
    /// The column's entries.
    pub column_entries: u64,
    /// The position of the first entry the two disagree at, where they do.
    pub first_difference: Option<u64>,
    /// A key the index names two live entities for, where it does, with the two.
    pub shared_key: Option<(UniqueKey, u32, u32)>,
}

/// Compare the runs at `index`, less every entity in `retired`, with the runs at `column`, written
/// in key order with disjoint ranges, entry by entry. Each side is read once, a page at a time.
pub fn compare_unique_runs(
    kind: KeyKind,
    index: &[PathBuf],
    retired: &croaring::Bitmap,
    column: &[PathBuf],
) -> Result<IndexComparison> {
    match kind {
        KeyKind::Keyword => compare_runs::<u128>(index, retired, column, UniqueKey::Keyword),
        KeyKind::Unsigned | KeyKind::Signed => {
            compare_runs::<u64>(index, retired, column, UniqueKey::Int)
        }
    }
}

fn compare_runs<K: crate::key_index::Key>(
    index: &[PathBuf],
    retired: &croaring::Bitmap,
    column: &[PathBuf],
    wrap: fn(K) -> UniqueKey,
) -> Result<IndexComparison> {
    let column_runs = column
        .iter()
        .map(|path| KeyRun::<K>::open_sequential(path))
        .collect::<Result<Vec<_>>>()?;
    let mut column_entries = column_runs.iter().flat_map(KeyRun::iter);
    let mut out = IndexComparison {
        index_entries: 0,
        column_entries: 0,
        first_difference: None,
        shared_key: None,
    };
    let mut previous: Option<(K, u32)> = None;
    crate::key_index::for_each_merged::<K>(
        index,
        |_, entity| retired.contains(entity),
        |key, entity| {
            if let Some((held, other)) = previous {
                if held == key && out.shared_key.is_none() {
                    out.shared_key = Some((wrap(key), other, entity));
                }
            }
            previous = Some((key, entity));
            let at = out.index_entries;
            out.index_entries += 1;
            let theirs = column_entries.next().transpose()?;
            if theirs.is_some() {
                out.column_entries += 1;
            }
            if theirs != Some((key, entity)) && out.first_difference.is_none() {
                out.first_difference = Some(at);
            }
            Ok(())
        },
    )?;
    for entry in column_entries {
        entry?;
        if out.first_difference.is_none() {
            out.first_difference = Some(out.column_entries);
        }
        out.column_entries += 1;
    }
    Ok(out)
}

/// Every entry of the run at `path`, in `(key, entity)` order, to `visit`. Every page is checked
/// as it is read.
pub fn for_each_entry(
    kind: KeyKind,
    path: &Path,
    mut visit: impl FnMut(UniqueKey, u32) -> Result<()>,
) -> Result<()> {
    match open_run(kind, path)? {
        AnyRun::Int(run) => {
            for entry in run.iter() {
                let (k, entity) = entry?;
                visit(UniqueKey::Int(k), entity)?;
            }
        }
        AnyRun::Keyword(run) => {
            for entry in run.iter() {
                let (k, entity) = entry?;
                visit(UniqueKey::Keyword(k), entity)?;
            }
        }
    }
    Ok(())
}

enum AnyRun {
    Int(KeyRun<u64>),
    Keyword(KeyRun<u128>),
}

fn open_run(kind: KeyKind, path: &Path) -> Result<AnyRun> {
    Ok(match kind.is_keyword() {
        true => AnyRun::Keyword(KeyRun::open(path)?),
        false => AnyRun::Int(KeyRun::open(path)?),
    })
}

impl AnyRun {
    /// `(position in keys, entity)` for every entity stored under each of `keys`, which are
    /// ascending and of this run's width.
    fn lookup_sorted(&self, keys: &[UniqueKey]) -> Result<Vec<(usize, u32)>> {
        match self {
            AnyRun::Int(run) => {
                let keys: Vec<u64> = keys
                    .iter()
                    .map(|k| match k {
                        UniqueKey::Int(k) => Ok(*k),
                        UniqueKey::Keyword(_) => Err(width_mismatch()),
                    })
                    .collect::<Result<_>>()?;
                run.lookup_sorted(&keys)
            }
            AnyRun::Keyword(run) => {
                let keys: Vec<u128> = keys
                    .iter()
                    .map(|k| match k {
                        UniqueKey::Keyword(k) => Ok(*k),
                        UniqueKey::Int(_) => Err(width_mismatch()),
                    })
                    .collect::<Result<_>>()?;
                run.lookup_sorted(&keys)
            }
        }
    }
}

/// One run of an index, opened at its first lookup and shared with every later index that names
/// the same file.
struct Slot {
    rel: String,
    path: PathBuf,
    kind: KeyKind,
    run: OnceLock<AnyRun>,
}

impl Slot {
    fn new(rel: &str, prefix_dir: &Path, kind: KeyKind) -> Slot {
        Slot {
            rel: rel.to_string(),
            path: prefix_dir.join(rel),
            kind,
            run: OnceLock::new(),
        }
    }

    fn run(&self) -> Result<&AnyRun> {
        if let Some(run) = self.run.get() {
            return Ok(run);
        }
        let opened = open_run(self.kind, &self.path)?;
        // Two lookups racing to the first open both open the file; one mapping is kept.
        let _ = self.run.set(opened);
        Ok(self.run.get().expect("set above"))
    }
}

/// One unique column's index: its base runs, with their key ranges, and its live runs.
pub struct UniqueIndex {
    kind: KeyKind,
    /// Ascending by range and disjoint.
    base: Vec<(u128, u128, Arc<Slot>)>,
    live: Vec<Arc<Slot>>,
}

impl UniqueIndex {
    /// The index `runs` describes under `prefix_dir`, sharing each run `reuse` already holds by
    /// the same path. Reads no file.
    pub fn open(
        runs: &UniqueIndexRuns,
        kind: KeyKind,
        prefix_dir: &Path,
        reuse: Option<&UniqueIndex>,
    ) -> Result<UniqueIndex> {
        let held = |rel: &str| -> Arc<Slot> {
            reuse
                .and_then(|index| index.slots().find(|slot| slot.rel == rel && slot.kind == kind))
                .cloned()
                .unwrap_or_else(|| Arc::new(Slot::new(rel, prefix_dir, kind)))
        };
        let malformed = |detail: String| StoreError::MalformedBundle { detail };
        let mut base = Vec::with_capacity(runs.base.len());
        for run in &runs.base {
            let parse = |hex: &str| {
                u128::from_str_radix(hex, 16).map_err(|_| {
                    malformed(format!(
                        "unique index '{}': base run {} has key bound '{hex}', which is not hex",
                        runs.attribute, run.path
                    ))
                })
            };
            let (lo, hi) = (parse(&run.first_key)?, parse(&run.last_key)?);
            if lo > hi {
                return Err(malformed(format!(
                    "unique index '{}': base run {} has its first key after its last",
                    runs.attribute, run.path
                )));
            }
            base.push((lo, hi, held(&run.path)));
        }
        if base.windows(2).any(|w| w[0].1 >= w[1].0) {
            return Err(malformed(format!(
                "unique index '{}': base runs overlap or are out of order; list them in ascending \
                 key order with disjoint ranges",
                runs.attribute
            )));
        }
        Ok(UniqueIndex {
            kind,
            base,
            live: runs.live.iter().map(|rel| held(rel)).collect(),
        })
    }

    fn slots(&self) -> impl Iterator<Item = &Arc<Slot>> {
        self.base.iter().map(|(_, _, slot)| slot).chain(&self.live)
    }

    pub fn kind(&self) -> KeyKind {
        self.kind
    }

    /// Every entity stored under each of `keys`, in any order and possibly repeated, as
    /// `(position in keys, entity)`, ascending by position. An entity found in two runs is
    /// reported once.
    pub fn lookup(&self, keys: &[UniqueKey]) -> Result<Vec<(usize, u32)>> {
        let mut order: Vec<usize> = (0..keys.len()).collect();
        order.sort_by_key(|&i| keys[i]);
        let sorted: Vec<UniqueKey> = order.iter().map(|&i| keys[i]).collect();
        let mut out: Vec<(usize, u32)> = Vec::new();
        for slot in &self.live {
            for (i, entity) in slot.run()?.lookup_sorted(&sorted)? {
                out.push((order[i], entity));
            }
        }
        for (lo, hi, slot) in &self.base {
            let from = sorted.partition_point(|k| k.widen() < *lo);
            let to = sorted.partition_point(|k| k.widen() <= *hi);
            if from >= to {
                continue;
            }
            for (i, entity) in slot.run()?.lookup_sorted(&sorted[from..to])? {
                out.push((order[from + i], entity));
            }
        }
        out.sort_unstable();
        out.dedup();
        Ok(out)
    }

    /// The base runs' paths, in key order, and the live runs', prefix-relative.
    pub fn run_paths(&self) -> (Vec<&str>, Vec<&str>) {
        (
            self.base.iter().map(|(_, _, s)| s.rel.as_str()).collect(),
            self.live.iter().map(|s| s.rel.as_str()).collect(),
        )
    }
}

/// Every unique column's index in one partition, by attribute name.
#[derive(Default)]
pub struct UniqueIndexes {
    by_attribute: BTreeMap<String, UniqueIndex>,
}

impl UniqueIndexes {
    /// The indexes `segments` lists, typed by `manifest`'s declarations, sharing every run
    /// `reuse` holds. Reads no file. An index for a column the manifest does not declare, or
    /// declares at a type that cannot be unique, is refused.
    pub fn open(
        manifest: &Manifest,
        segments: &SegmentsManifest,
        prefix_dir: &Path,
        reuse: Option<&UniqueIndexes>,
    ) -> Result<UniqueIndexes> {
        let mut by_attribute = BTreeMap::new();
        for runs in &segments.unique_indexes {
            let kind = manifest
                .declared_scalars
                .iter()
                .find(|d| d.name == runs.attribute)
                .and_then(|d| KeyKind::of(d.arrow_type))
                .ok_or_else(|| StoreError::MalformedBundle {
                    detail: format!(
                        "a unique index names attribute '{}', which the manifest does not \
                         declare as a keyword, integer or timestamp column",
                        runs.attribute
                    ),
                })?;
            let held = reuse.and_then(|r| r.get(&runs.attribute));
            by_attribute.insert(
                runs.attribute.clone(),
                UniqueIndex::open(runs, kind, prefix_dir, held)?,
            );
        }
        Ok(UniqueIndexes { by_attribute })
    }

    pub fn get(&self, attribute: &str) -> Option<&UniqueIndex> {
        self.by_attribute.get(attribute)
    }

    pub fn is_empty(&self) -> bool {
        self.by_attribute.is_empty()
    }
}

/// The served schema's unique flags, set from the indexes the partition manifest lists: a column
/// is unique exactly where an index exists for it. The side manifest is newer than
/// `MANIFEST.json`, so a declaration made or removed at a running service shows here first.
pub fn with_unique_flags(manifest: &Manifest, segments: &SegmentsManifest) -> Manifest {
    let mut manifest = manifest.clone();
    for declared in &mut manifest.declared_scalars {
        declared.unique = segments
            .unique_indexes
            .iter()
            .any(|runs| runs.attribute == declared.name);
    }
    manifest
}

/// How many values a refusal names at most.
pub const DUPLICATE_EXAMPLES: usize = 10;

/// A value as a refusal names it: a keyword quoted, a number as its digits.
pub fn value_text(value: &ScalarValue) -> String {
    use ScalarValue as V;
    match value {
        V::Utf8(s) => format!("'{s}'"),
        V::U8(v) => v.to_string(),
        V::U16(v) => v.to_string(),
        V::U32(v) => v.to_string(),
        V::U64(v) => v.to_string(),
        V::I8(v) => v.to_string(),
        V::I16(v) => v.to_string(),
        V::I32(v) => v.to_string(),
        V::I64(v) | V::TimestampUs(v) => v.to_string(),
        other => format!("{other:?}"),
    }
}

/// An integer key as a refusal names its value. `None` for a keyword key, a hash whose text is
/// read from where the value is stored.
pub fn key_text(key: UniqueKey, kind: KeyKind) -> Option<String> {
    match (key, kind) {
        (UniqueKey::Int(k), KeyKind::Unsigned) => Some(k.to_string()),
        (UniqueKey::Int(k), _) => Some(crate::key_index::signed_value(k).to_string()),
        (UniqueKey::Keyword(_), _) => None,
    }
}

/// The refusal of a column declared unique that holds values more than one item holds: how many
/// such values, and up to [`DUPLICATE_EXAMPLES`] of them. The build and the running service refuse
/// in these words.
pub fn duplicates_message(attribute: &str, count: u64, examples: &[String]) -> String {
    let noun = match count {
        1 => "value is",
        _ => "values are",
    };
    format!(
        "attribute '{attribute}' is declared unique and {count} {noun} held by more than one \
         item, such as {}; remove the duplicates or declare it without `unique`",
        examples.join(", ")
    )
}

/// The declared columns that are unique, with their positions in the declaration.
pub fn unique_columns(declared: &[DeclaredScalar]) -> impl Iterator<Item = (usize, &DeclaredScalar)> {
    declared.iter().enumerate().filter(|(_, d)| d.unique)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn integer_keys_are_the_same_at_every_width_and_refuse_values_the_type_cannot_hold() {
        assert_eq!(
            key_of(ScalarType::U8, &ScalarValue::U8(7)),
            key_of(ScalarType::U64, &ScalarValue::U64(7))
        );
        assert_eq!(key_of_integer(ScalarType::U8, 7), Some(UniqueKey::unsigned(7)));
        assert_eq!(key_of_integer(ScalarType::U8, 256), None);
        assert_eq!(key_of_integer(ScalarType::U64, -1), None);
        assert_eq!(key_of_integer(ScalarType::I64, -1), Some(UniqueKey::signed(-1)));
        assert_eq!(
            key_of_integer(ScalarType::U64, u64::MAX as i128),
            Some(UniqueKey::unsigned(u64::MAX))
        );
        assert_eq!(key_of(ScalarType::U64, &ScalarValue::Null), None);
        assert_eq!(
            key_of(ScalarType::Keyword, &ScalarValue::Utf8("a".into())),
            Some(UniqueKey::keyword("a"))
        );
    }

    fn runs(dir: &Path, stem: &str, entries: &[(UniqueKey, u32)]) -> Vec<WrittenUniqueRun> {
        let mut sorted = entries.to_vec();
        sorted.sort_unstable();
        write_sorted_runs(KeyKind::Unsigned, dir, stem, &sorted).unwrap()
    }

    #[test]
    fn a_lookup_gathers_every_run_its_key_may_be_in() {
        let tmp = tempfile::tempdir().unwrap();
        let prefix = tmp.path();
        let dir = prefix.join("u");
        let k = UniqueKey::unsigned;
        let base = runs(&dir, "base", &[(k(1), 10), (k(5), 11), (k(9), 12)]);
        let live = runs(&dir, "live", &[(k(5), 20), (k(7), 21)]);
        let listed = UniqueIndexRuns {
            attribute: "id".into(),
            base: base.iter().map(|r| r.as_base(prefix).unwrap()).collect(),
            live: live
                .iter()
                .map(|r| relative(prefix, &r.path).unwrap())
                .collect(),
        };
        let index = UniqueIndex::open(&listed, KeyKind::Unsigned, prefix, None).unwrap();
        let found = index.lookup(&[k(9), k(5), k(2), k(7)]).unwrap();
        assert_eq!(found, vec![(0, 12), (1, 11), (1, 20), (3, 21)]);
        let again = UniqueIndex::open(&listed, KeyKind::Unsigned, prefix, Some(&index)).unwrap();
        assert!(Arc::ptr_eq(&again.live[0], &index.live[0]));
    }
}
