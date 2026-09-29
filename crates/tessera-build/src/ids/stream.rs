//! The rule applied to a build's files by sorting them, one file at a time.
//!
//! For each file, in declaration order:
//!
//! 1. **Scan.** Each unique field the file carries goes into an external sort as `(key, row)`
//!    ([`KeySpill`]). A row carrying a `tessera_id` is refused here.
//! 2. **Merge.** Each field's sorted rows are merged against the run of `(key, item)` the earlier
//!    files built for that field. A row whose key is held names that item. Where the file carries
//!    one field, the later rows of a run of equal keys are refused here, one item or one value
//!    twice; where it carries several, one pass in row order decides them once every field has
//!    been merged, so that a refused row claims neither its item nor its values. Every decision
//!    is routed by row to a partition.
//! 3. **Walk.** The partition is replayed in row order into the file's numbers, deciding each row
//!    by [`name_row`] over the items its fields named, and numbering the rows that create items in
//!    row order.
//! 4. **Holdings.** The keys the file's accepted rows gave items become a new run for later files.
//!
//! Every pass is sequential in the file's row order or in key order, and holds no more memory than
//! its sorts' share of the budget whatever the file holds: in a file carrying several fields, the
//! rows of each unset value that several rows carry go through a sort of their own, and which of
//! them sets it is kept as a bit on disk. A file whose rows all create items, with nothing refused,
//! is numbered by its row position alone and writes no numbers: the first view of a corpus whose
//! values are unique costs one sort per field, and its sorted keys are the next file's holdings as
//! they stand.

use std::cell::Cell;
use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::time::SystemTime;

use super::run::{RunReader, RunReceipt, RunWriter};
use rayon::prelude::*;
use tessera_lifecycle::resolve::{self, Batch, Named, Refusal};
use tessera_store::key_index::{Key, KeySpill};
use tessera_store::unique::{KeyKind, UniqueKey};
use tessera_types::EntityId;

use super::report::{Tally, OUTSIDE_LIMIT};
use super::scan::{FileRead, Scanned};
use super::{
    check_cap, CarriedField, Limit, Limited, Numbering, Numbers, Read, ReadInput, ReadRows,
};
use crate::error::{BuildError, Result};
use crate::row_groups::FileGroups;
use crate::spill::{self, boundaries_uniform, mix64, MappedArray, Partition};

/// A row the walk has not decided, in a file's numbers.
const PENDING: u32 = u32::MAX;

/// A row the walk has not decided and `--limit` leaves out unless it names an item, in a file's
/// numbers.
const LEFT_OUT: u32 = u32::MAX - 1;

/// A decision about one row, routed by row: an item it names (`item + 1`), or a refusal.
const UNKNOWN_TESSERA_ID: u32 = u32::MAX;
const ONE_ITEM_TWICE: u32 = u32::MAX - 1;
const ONE_VALUE_TWICE: u32 = u32::MAX - 2;

/// How many rows of a names file are compared with the creating file's before a file in which
/// fewer than half matched stops being compared.
const ZIP_TRIAL: u64 = 1 << 20;

/// The share of the memory budget one field's sort holds, and its bounds.
const SORT_SHARE: u64 = 4;
const SORT_MIN: u64 = 4 << 20;
const SORT_MAX: u64 = 4 << 30;

/// Number every file of the build.
pub(crate) fn number(args: &crate::BuildArgs, tmp: &Path) -> Result<Numbering> {
    number_with(args, tmp, ZIP_TRIAL)
}

/// [`number`], a names file compared row by row with its creating file for `zip_trial` rows before
/// one in which fewer than half matched stops being compared.
pub(super) fn number_with(
    args: &crate::BuildArgs,
    tmp: &Path,
    zip_trial: u64,
) -> Result<Numbering> {
    let budget = args
        .memory_budget
        .unwrap_or_else(crate::pipeline::detect_memory_budget);
    let mut pass = Pass {
        tmp: tmp.to_path_buf(),
        sort_bytes: (budget / SORT_SHARE).clamp(SORT_MIN, SORT_MAX) as usize,
        held: BTreeMap::new(),
        limited: Limit::of(&args.schema, args.limit)?.map(Limited::new),
        zip_trial,
        next: 0,
        sequence: 0,
    };
    let mut numbering = Numbering {
        reads: Vec::new(),
        items: 0,
        refused: Vec::new(),
    };
    for read in super::reads(args)? {
        let (rows, refused) = pass.read(args, &read)?;
        if args.strict && refused.iter().any(super::RefusedRows::is_refusal) {
            return Err(strict_refusal(&refused));
        }
        numbering.refused.extend(refused);
        numbering.reads.push((read.kind, rows));
    }
    numbering.items = pass.next;
    Ok(numbering)
}

/// `--strict`'s refusal, naming the first file and reason the rule refused rows for.
pub(super) fn strict_refusal(refused: &[super::RefusedRows]) -> BuildError {
    let first: Vec<super::RefusedRows> = refused
        .iter()
        .filter(|entry| entry.is_refusal())
        .take(1)
        .cloned()
        .collect();
    BuildError::Invalid(format!(
        "--strict: {}. Remove or correct those rows, or build without --strict to refuse only them",
        super::report::describe(&first)[0]
    ))
}

/// The run of `(key, item)` the files read so far gave one field.
struct HeldRun {
    receipt: RunReceipt,
    /// Where the run's second column is a row of a file whose rows are items `base + row`.
    base: Option<u32>,
    /// That file, where it is one every row of which created an item and gave it its value.
    source: Option<ZipSource>,
}

/// A file whose row `r` created item `base + r` with its value of `field`: a file naming items by
/// the same field in the same order is compared with it row by row, and only the rows that differ
/// are sorted.
#[derive(Clone)]
struct ZipSource {
    path: PathBuf,
    field: CarriedField,
    /// Its rows when it was numbered.
    rows: u64,
    stamp: Stamp,
}

/// A file's length and modification time, which a rewrite of the file changes.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) struct Stamp {
    len: u64,
    modified: Option<SystemTime>,
}

impl Stamp {
    pub(super) fn of(path: &Path) -> Result<Stamp> {
        let metadata = std::fs::metadata(path).map_err(|e| BuildError::io(path, e))?;
        Ok(Stamp {
            len: metadata.len(),
            modified: metadata.modified().ok(),
        })
    }

    /// Refuse a file that is not the one stamped, `rows` then and `now` rows now.
    pub(super) fn check(&self, path: &Path, rows: u64, now: u64) -> Result<()> {
        if rows != now || Stamp::of(path)? != *self {
            return Err(BuildError::Invalid(format!(
                "{} changed while the build read it: it held {rows} rows of {} bytes and holds \
                 {now} rows of {} bytes. Build again from files that do not change",
                path.display(),
                self.len,
                Stamp::of(path)?.len,
            )));
        }
        Ok(())
    }
}

/// One field's keys of a file, row after row, decoded on a thread of their own.
struct KeyStream {
    receiver: Option<std::sync::mpsc::Receiver<Result<Vec<Option<UniqueKey>>>>>,
    producer: Option<std::thread::JoinHandle<()>>,
    batch: Vec<Option<UniqueKey>>,
    first: u64,
}

impl KeyStream {
    fn open(source: &ZipSource) -> Result<KeyStream> {
        let (sender, receiver) = std::sync::mpsc::sync_channel(8);
        let source = source.clone();
        let producer = std::thread::Builder::new()
            .name("identity-keys".to_string())
            .spawn(move || {
                let read = || -> Result<()> {
                    let groups = FileGroups::open(&source.path)?;
                    source
                        .stamp
                        .check(&source.path, source.rows, groups.rows())?;
                    let (index, _) = groups
                        .schema()
                        .column_with_name(&source.field.column)
                        .ok_or_else(|| BuildError::Schema {
                            path: source.path.clone(),
                            detail: format!("missing required column '{}'", source.field.column),
                        })?;
                    let projection = groups.projection(&[index]);
                    let all: Vec<usize> = (0..groups.count()).collect();
                    groups.each_batch(&source.path, &all, &projection, |_, batch| {
                        let keys =
                            super::scan::keys_of(&source.path, batch.column(0), &source.field)?;
                        Ok(match sender.send(Ok(keys)) {
                            Ok(()) => std::ops::ControlFlow::Continue(()),
                            Err(_) => std::ops::ControlFlow::Break(()),
                        })
                    })
                };
                if let Err(e) = read() {
                    let _ = sender.send(Err(e));
                }
            })
            .map_err(|e| BuildError::io(Path::new("identity-keys"), e))?;
        Ok(KeyStream {
            receiver: Some(receiver),
            producer: Some(producer),
            batch: Vec::new(),
            first: 0,
        })
    }

    /// The key at `row`, rows asked in ascending order; `None` past the file's last row.
    fn key_at(&mut self, row: u64) -> Result<Option<Option<UniqueKey>>> {
        while row >= self.first + self.batch.len() as u64 {
            let Some(receiver) = &self.receiver else {
                return Ok(None);
            };
            match receiver.recv() {
                Ok(keys) => {
                    self.first += self.batch.len() as u64;
                    self.batch = keys?;
                }
                Err(_) => {
                    self.receiver = None;
                    return Ok(None);
                }
            }
        }
        Ok(Some(self.batch[(row - self.first) as usize]))
    }
}

impl Drop for KeyStream {
    fn drop(&mut self) {
        self.receiver = None;
        if let Some(producer) = self.producer.take() {
            let _ = producer.join();
        }
    }
}

/// What the files read so far hold, per unique field. The held runs' files go with it.
struct Pass {
    tmp: PathBuf,
    sort_bytes: usize,
    held: BTreeMap<u16, Vec<HeldRun>>,
    limited: Option<Limited>,
    /// [`ZIP_TRIAL`], lowered by tests.
    zip_trial: u64,
    next: u64,
    sequence: u64,
}

impl Drop for Pass {
    fn drop(&mut self) {
        for run in self.held.values().flatten() {
            let _ = std::fs::remove_file(&run.receipt.path);
        }
    }
}

/// One file's numbers as they are decided.
enum Rows {
    Mapped(MappedArray<u32>),
    Held(Vec<u32>),
}

impl Rows {
    fn slice(&mut self) -> &mut [u32] {
        match self {
            Rows::Mapped(rows) => rows.as_mut_slice(),
            Rows::Held(rows) => rows,
        }
    }

    fn numbers(self) -> Numbers {
        match self {
            Rows::Mapped(rows) => Numbers::Mapped(rows),
            Rows::Held(rows) => Numbers::Held(rows),
        }
    }
}

/// One field's sort, at its keys' width.
enum FieldSort {
    Int(KeySpill<u64>),
    Keyword(KeySpill<u128>),
}

impl FieldSort {
    fn push(&mut self, key: UniqueKey, row: u32) -> Result<()> {
        let store = |e: tessera_store::StoreError| BuildError::Invalid(e.to_string());
        match (self, key) {
            (FieldSort::Int(sort), UniqueKey::Int(k)) => sort.push(k, row).map_err(store),
            (FieldSort::Keyword(sort), UniqueKey::Keyword(k)) => sort.push(k, row).map_err(store),
            _ => Err(BuildError::Invalid(
                "a unique field's key arrived at the other width".to_string(),
            )),
        }
    }
}

/// What merging one field against the holdings found.
struct Merged {
    /// The file's `(key, row)` for keys nobody held, sorted: the values its accepted rows may set.
    unset: Option<RunReceipt>,
    /// How many unset values several rows carry, in a file carrying several fields: which of those
    /// rows sets each is decided once every field is merged.
    candidates: u64,
}

/// A row of an unset value several rows carry, as the candidates' sort keys it: in row order, and
/// a row's values in the order of the fields the file carries. The entry beside it is the value's
/// number among its field's candidates.
fn candidate_key(row: u32, field: usize) -> u64 {
    u64::from(row) << 16 | field as u64
}

impl Pass {
    /// A file's numbers, every row no item until the pass decides it: mapped for a file, held for
    /// the few members a declaration or an artifacts file lists.
    fn rows(&mut self, input: &Input<'_>, total: u64) -> Result<Rows> {
        Ok(match input {
            Input::File(_) => {
                let name = self.name("numbers");
                Rows::Mapped(MappedArray::zeroed(&self.tmp, &name, total as usize)?)
            }
            Input::Lists(..) => Rows::Held(vec![0; total as usize]),
        })
    }

    /// A name for a file of this pass's own, unique in the build's scratch directory.
    fn name(&mut self, stem: &str) -> String {
        self.sequence += 1;
        format!("identity-{stem}-{}", self.sequence)
    }

    fn scratch(&mut self, stem: &str) -> PathBuf {
        let name = self.name(stem);
        self.tmp.join(name)
    }

    fn read(
        &mut self,
        args: &crate::BuildArgs,
        read: &Read,
    ) -> Result<(ReadRows, Vec<super::RefusedRows>)> {
        match &read.input {
            ReadInput::File {
                path,
                fields,
                select,
            } => {
                let groups = FileGroups::open(path)?;
                let carried = super::carried_unique(groups.schema(), fields, &args.schema)
                    .map_err(|detail| BuildError::Schema {
                        path: path.clone(),
                        detail,
                    })?;
                let creates = read.batch == Batch::Creates;
                let limit = self
                    .limited
                    .as_ref()
                    .and_then(|l| l.read(&carried, creates));
                let file = FileRead::new(path, &groups, &carried, select.as_ref(), limit);
                if !creates {
                    resolve::require_identifier(file.tessera, carried.len()).map_err(|_| {
                        BuildError::Invalid(super::no_identifier(&read.object, path))
                    })?;
                }
                if groups.rows() >= u32::MAX as u64 {
                    return Err(BuildError::Invalid(format!(
                        "{}: {} holds {} rows, and a build reads at most {} rows of one file",
                        read.object,
                        path.display(),
                        groups.rows(),
                        u32::MAX - 1
                    )));
                }
                let (rows, tally) = self.number_rows(read, Input::File(&file))?;
                let values = file.values_at(&tally.sampled())?;
                let refused = tally.finish(&read.source, &read.object, |row| {
                    values
                        .get(&row)
                        .cloned()
                        .unwrap_or_else(|| format!("row {row}"))
                });
                Ok((
                    ReadRows {
                        groups: file.kept_groups(),
                        ..rows
                    },
                    refused,
                ))
            }
            ReadInput::Lists(lists) => {
                let scanned = Limited::lists(self.limited.as_ref(), lists);
                let (rows, tally) = self.number_rows(read, Input::Lists(lists, &scanned))?;
                let refused = tally.finish(&read.source, &read.object, |row| {
                    lists.texts[row as usize].clone()
                });
                Ok((rows, refused))
            }
        }
    }

    fn number_rows(&mut self, read: &Read, input: Input<'_>) -> Result<(ReadRows, Tally)> {
        let (carried, total, takes_every_row, tessera) = match &input {
            Input::File(file) => (
                file.carried,
                file.groups.rows(),
                file.takes_every_row(),
                file.tessera,
            ),
            Input::Lists(lists, _) => (lists.carried.as_slice(), lists.len() as u64, true, false),
        };
        let base = self.next;
        let creates = read.batch == Batch::Creates;
        let single = carried.len() == 1;
        let sets_values = matches!(read.batch, Batch::Creates | Batch::Edits);
        let one_row_per_item = read.batch != Batch::Names;
        // Where the file carries several fields, what is left to decide once every field is merged
        // is decided after the walk: one item twice, and which row sets a value several carry.
        let deferred = !single && (one_row_per_item || sets_values);
        let sorts_candidates = sets_values && !single;
        let mut tally = Tally::default();

        // A file of new items with nothing to name them by: each row is the next item.
        if creates && carried.is_empty() && !tessera && takes_every_row {
            return self.offset(base, total).map(|rows| (rows, tally));
        }

        // ---- 1. scan -------------------------------------------------------------------------
        let lazy = creates && takes_every_row;
        // The file as the scan reads it, for a later file compared with it row by row.
        let stamp = match &input {
            Input::File(file) if lazy => Some(Stamp::of(file.path)?),
            _ => None,
        };
        let mut rows: Option<Rows> = match lazy {
            true => None,
            false => Some(self.rows(&input, total)?),
        };
        let updates_path = self.scratch("decided");
        std::fs::create_dir_all(&updates_path).map_err(|e| BuildError::io(&updates_path, e))?;
        let boundaries = boundaries_uniform(total);
        let mut updates = Partition::create(&updates_path, "row", boundaries.clone(), 8, total)?;
        let mut decided = 0u64;
        // The sorts held at once share the sort's part of the budget: every field's while the
        // file is merged, and after it, the candidates', the named items' and the refused rows'.
        let spills = match deferred {
            true => (carried.len() + usize::from(sorts_candidates)).max(3),
            false => carried.len().max(1),
        };
        let share = (self.sort_bytes / spills).max(SORT_MIN as usize);
        let store = |e: tessera_store::StoreError| BuildError::Invalid(e.to_string());
        let mut sorts: Vec<FieldSort> = carried
            .iter()
            .map(|field| {
                Ok(match KeyKind::of(field.ty) {
                    Some(KeyKind::Keyword) => {
                        FieldSort::Keyword(KeySpill::create(&self.tmp, share).map_err(store)?)
                    }
                    _ => FieldSort::Int(KeySpill::create(&self.tmp, share).map_err(store)?),
                })
            })
            .collect::<Result<_>>()?;
        // A names file carrying the one field a single creating file set, compared with that file
        // row by row: a row whose value is the one at its position there names that row's item.
        let zip = match (&input, read.batch) {
            (Input::File(_), Batch::Names) if single && !tessera => {
                match self.held.get(&carried[0].position).map(Vec::as_slice) {
                    Some(
                        [HeldRun {
                            base: Some(zip_base),
                            source: Some(source),
                            ..
                        }],
                    ) => Some((KeyStream::open(source)?, *zip_base)),
                    _ => None,
                }
            }
            _ => None,
        };
        let ordered = zip.is_some();
        let comparing = Cell::new(ordered);
        let mut zip = zip;
        // Rows compared and rows that matched: a file in another order stops being compared.
        let (mut compared, mut matched) = (0u64, 0u64);
        let mut on_batch = |scanned: &Scanned| -> Result<()> {
            let numbers = rows.as_mut().map(Rows::slice);
            let mut numbers = numbers;
            for offset in 0..scanned.len {
                let row = scanned.first + offset as u64;
                let selected = scanned.selected.as_ref().is_none_or(|s| s[offset]);
                if let Some(numbers) = numbers.as_deref_mut() {
                    numbers[row as usize] = match selected {
                        false => 0,
                        true if scanned.left_out.as_ref().is_some_and(|l| l[offset]) => LEFT_OUT,
                        true => PENDING,
                    };
                }
                if !selected {
                    if scanned.outside.as_ref().is_some_and(|o| o[offset]) {
                        tally.refuse(OUTSIDE_LIMIT, row);
                    }
                    continue;
                }
                if scanned.tessera.as_ref().is_some_and(|t| t[offset]) {
                    push(&mut updates, row as u32, UNKNOWN_TESSERA_ID)?;
                    decided += 1;
                    continue;
                }
                if let (Some((stream, zip_base)), Some(numbers)) =
                    (zip.as_mut(), numbers.as_deref_mut())
                {
                    if let Some(key) = scanned.keys[0][offset] {
                        compared += 1;
                        let hit = stream.key_at(row)? == Some(Some(key));
                        if hit {
                            numbers[row as usize] = *zip_base + row as u32 + 1;
                            matched += 1;
                        }
                        if compared == self.zip_trial && matched * 2 < compared {
                            zip = None;
                            comparing.set(false);
                        }
                        if hit {
                            continue;
                        }
                    }
                }
                for (sort, keys) in sorts.iter_mut().zip(&scanned.keys) {
                    if let Some(key) = keys[offset] {
                        sort.push(key, row as u32)?;
                    }
                }
            }
            Ok(())
        };
        match &input {
            Input::File(file) if ordered => {
                file.scan_ordered(|| comparing.get(), |scanned| on_batch(&scanned))?
            }
            Input::File(file) => file.scan(|scanned| on_batch(&scanned))?,
            Input::Lists(_, scanned) => on_batch(scanned)?,
        }

        // ---- 2. merge each field against what earlier files hold ------------------------------
        let mut candidates: Option<KeySpill<u64>> = match sorts_candidates {
            true => Some(KeySpill::create(&self.tmp, share).map_err(store)?),
            false => None,
        };
        let mut merged: Vec<Merged> = Vec::with_capacity(carried.len());
        for (index, (field, sort)) in carried.iter().zip(sorts).enumerate() {
            let context = FieldContext {
                single,
                creates,
                sets_values,
                one_row_per_item,
            };
            let unset_path = self.scratch("unset");
            let runs = self
                .held
                .get(&field.position)
                .map_or(&[][..], Vec::as_slice);
            let merge = Merge {
                index,
                context,
                unset_path: &unset_path,
                updates: &mut updates,
                candidates: candidates.as_mut(),
            };
            let outcome = match sort {
                FieldSort::Int(sort) => merge_field::<u64>(sort, runs, merge)?,
                FieldSort::Keyword(sort) => merge_field::<u128>(sort, runs, merge)?,
            };
            decided += outcome.1;
            merged.push(outcome.0);
        }
        let mut updates = updates.finish()?;

        // A file of new items that named nothing and was refused nothing: row `r` is `base + r`.
        if lazy && decided == 0 && merged.iter().all(|m| m.candidates == 0) {
            let offset = self.offset(base, total)?;
            for (field, outcome) in carried.iter().zip(merged) {
                if let Some(unset) = outcome.unset {
                    let source = match (&input, &stamp) {
                        (Input::File(file), Some(stamp)) => Some(ZipSource {
                            path: file.path.to_path_buf(),
                            field: field.clone(),
                            rows: total,
                            stamp: stamp.clone(),
                        }),
                        _ => None,
                    };
                    if let Some(limited) = self.limited.as_mut().filter(|_| unset.count() > 0) {
                        limited.given(field.position);
                    }
                    self.held.entry(field.position).or_default().push(HeldRun {
                        receipt: unset,
                        base: Some(base as u32),
                        source,
                    });
                }
            }
            return Ok((offset, tally));
        }
        let mut rows = match rows {
            Some(rows) => rows,
            None => self.rows(&input, total)?,
        };

        // ---- 3. walk the decisions in row order ------------------------------------------------
        // Where the file carries one field, nothing is left to decide once the walk has applied the
        // merge's decisions, so the walk numbers the rows too.
        let mut named_items: Option<KeySpill<u32>> = match deferred && one_row_per_item {
            true => Some(KeySpill::create(&self.tmp, share).map_err(store)?),
            false => None,
        };
        let mut numbering = FileNumbers::new(base, creates);
        walk(
            &mut rows,
            &mut updates,
            &boundaries,
            total,
            lazy,
            single,
            &mut tally,
            &mut named_items,
            (!deferred).then_some(&mut numbering),
        )?;
        drop(updates);

        // ---- 4. with several fields: one item twice, one value twice, then the numbers --------
        // Decided in one pass in row order, so that a row refused for any reason claims neither its
        // item nor its values. Each row naming an item is linked to the row before it naming that
        // item, and carries whether any row of that chain up to it was kept.
        if deferred {
            let numbers = rows.slice();
            let mut chains: Option<(MappedArray<u32>, MappedArray<u64>)> = None;
            if let Some(named) = named_items {
                let name = self.name("before");
                let mut before = MappedArray::<u32>::zeroed(&self.tmp, &name, total as usize)?;
                let links = before.as_mut_slice();
                let mut last: Option<(u32, u32)> = None;
                named
                    .drain(|item, row| {
                        if let Some((_, earlier)) = last.filter(|&(held, _)| held == item) {
                            links[row as usize] = earlier + 1;
                        }
                        last = Some((item, row));
                        Ok(())
                    })
                    .map_err(store)?;
                let words = total.div_ceil(64) as usize;
                let name = self.name("kept");
                let kept = MappedArray::<u64>::zeroed(&self.tmp, &name, words)?;
                chains = Some((before, kept));
            }
            let total_candidates: u64 = merged.iter().map(|m| m.candidates).sum();
            let name = self.name("claimed");
            let mut claimed = match total_candidates {
                0 => None,
                n => Some(MappedArray::<u64>::zeroed(
                    &self.tmp,
                    &name,
                    n.div_ceil(64) as usize,
                )?),
            };
            let mut step = |row: u32, bits: &[u64], tally: &mut Tally| {
                let stored = &mut numbers[row as usize];
                let names_item = !matches!(*stored, 0 | PENDING | LEFT_OUT);
                if let Some((before, kept)) = chains.as_mut().filter(|_| names_item) {
                    let earlier = before.as_mut_slice()[row as usize];
                    let kept = kept.as_mut_slice();
                    let is_kept = |row: u32| kept[(row / 64) as usize] & (1 << (row % 64)) != 0;
                    let claimed_before = earlier.checked_sub(1).is_some_and(is_kept);
                    let keep = match claimed_before {
                        true => {
                            *stored = 0;
                            tally.refuse(Refusal::ONE_ITEM_TWICE, u64::from(row));
                            true
                        }
                        false => {
                            if let Some(claimed) = claimed.as_mut() {
                                claim(stored, row, bits, claimed.as_mut_slice(), creates, tally);
                            }
                            *stored != 0
                        }
                    };
                    if keep {
                        kept[(row / 64) as usize] |= 1 << (row % 64);
                    }
                    return;
                }
                if let Some(claimed) = claimed.as_mut() {
                    claim(stored, row, bits, claimed.as_mut_slice(), creates, tally);
                }
            };
            // Each candidate value's bit: its field's first, plus its number in the field.
            let firsts: Vec<u64> = merged
                .iter()
                .scan(0u64, |sum, m| {
                    let first = *sum;
                    *sum += m.candidates;
                    Some(first)
                })
                .collect();
            let mut next = 0u32;
            if let Some(candidates) = candidates.filter(|_| total_candidates > 0) {
                let mut at: Option<u32> = None;
                let mut bits: Vec<u64> = Vec::new();
                candidates
                    .drain(|key, number| {
                        let row = (key >> 16) as u32;
                        if let Some(done) = at.filter(|&done| done != row) {
                            while next < done {
                                step(next, &[], &mut tally);
                                next += 1;
                            }
                            step(done, &bits, &mut tally);
                            next = done + 1;
                            bits.clear();
                        }
                        at = Some(row);
                        bits.push(firsts[(key & 0xffff) as usize] + u64::from(number));
                        Ok(())
                    })
                    .map_err(store)?;
                if let Some(done) = at {
                    while next < done {
                        step(next, &[], &mut tally);
                        next += 1;
                    }
                    step(done, &bits, &mut tally);
                    next = done + 1;
                }
            }
            while u64::from(next) < total {
                step(next, &[], &mut tally);
                next += 1;
            }
            drop(step);
            drop(chains);
            drop(claimed);
            let numbers = rows.slice();
            for (row, number) in numbers.iter_mut().enumerate() {
                numbering.decide(number, row as u64, &mut tally)?;
            }
            numbering.created.seal();
        }
        let created = numbering.created;
        self.next = base + created.count;

        // ---- 5. the values the accepted rows gave items become holdings -----------------------
        if sets_values {
            let numbers = rows.slice();
            for (field, outcome) in carried.iter().zip(merged) {
                let Some(unset) = outcome.unset else { continue };
                let kind = KeyKind::of(field.ty).expect("a unique field has a key kind");
                let limit = self
                    .limited
                    .as_ref()
                    .filter(|l| l.limit.position == field.position)
                    .map(|l| l.limit.clone());
                let (runs, retired, raised) = match kind {
                    KeyKind::Keyword => {
                        self.set_values::<u128>(unset, numbers, &created, base, limit.as_ref())?
                    }
                    _ => self.set_values::<u64>(unset, numbers, &created, base, limit.as_ref())?,
                };
                if let Some(limited) = self.limited.as_mut() {
                    if runs.iter().any(|run| run.receipt.count() > 0) {
                        limited.given(field.position);
                    }
                    if raised {
                        limited.raise();
                    }
                }
                let held_before = self
                    .held
                    .get(&field.position)
                    .is_some_and(|runs| !runs.is_empty());
                if held_before && !retired.is_empty() {
                    self.retire(field.position, kind, &retired)?;
                }
                self.held.entry(field.position).or_default().extend(runs);
            }
        }
        let (named, mixed) = numbering.anchor;
        Ok((
            ReadRows {
                numbers: rows.numbers(),
                groups: None,
                named,
                mixed,
            },
            tally,
        ))
    }

    /// A file whose row `r` is item `base + r`, every row of it.
    fn offset(&mut self, base: u64, total: u64) -> Result<ReadRows> {
        check_cap(base + total)?;
        self.next = base + total;
        use rayon::prelude::*;
        let mixed = (base..base + total)
            .into_par_iter()
            .map(mix64)
            .reduce(|| 0, u64::wrapping_add);
        Ok(ReadRows {
            numbers: Numbers::Offset { base: base as u32 },
            groups: None,
            named: total,
            mixed,
        })
    }

    /// Turn a file's unset `(key, row)` into runs of `(key, item)` for the rows the rule accepted,
    /// name the items whose value of this field changed, and say whether one was given a value at
    /// or above `limit`. The unset run is removed once read.
    ///
    /// A created row's item is its rank among the file's created rows. Every other row's item is
    /// its number, which is looked up a partition by row at a time, so the file's numbers are read
    /// in order; what that finds is sorted back into key order. So a file of edits costs two
    /// sequential passes over its values and no random reads, and its disk at any moment is the
    /// unset run beside what it is turned into, then one copy of the edits.
    // A record's width is the key type's, which `as_chunks` cannot take as a generic parameter.
    #[allow(clippy::chunks_exact_to_as_chunks)]
    fn set_values<K: Key>(
        &mut self,
        unset: RunReceipt,
        numbers: &[u32],
        created: &Created,
        base: u64,
        limit: Option<&Limit>,
    ) -> Result<(Vec<HeldRun>, croaring::Bitmap, bool)> {
        let store = |e: tessera_store::StoreError| BuildError::Invalid(e.to_string());
        let mut raised = false;
        let mut note = |key: K| {
            if limit.is_some_and(|limit| limit.beyond(key.widen() as u64)) {
                raised = true;
            }
        };
        let created_path = self.scratch("held");
        let mut created_run = RunWriter::<K>::create(&created_path)?;
        let by_row_path = self.scratch("named-by-row");
        std::fs::create_dir_all(&by_row_path).map_err(|e| BuildError::io(&by_row_path, e))?;
        let total = numbers.len() as u64;
        let boundaries = boundaries_uniform(total);
        let mut by_row = Partition::create(&by_row_path, "row", boundaries, 4 + K::WIDTH, total)?;
        let mut reader = RunReader::<K>::open(&unset)?;
        let mut record = [0u8; 20];
        while let Some((key, row)) = reader.next_entry()? {
            let row64 = u64::from(row);
            if created.contains(row64) {
                note(key);
                created_run.push(key, (base + created.rank(row64)) as u32)?;
                continue;
            }
            record[..4].copy_from_slice(&row.to_le_bytes());
            key.write(&mut record[4..4 + K::WIDTH]);
            by_row.push(&record[..4 + K::WIDTH])?;
        }
        drop(reader);
        let _ = std::fs::remove_file(&unset.path);
        let buckets = by_row.buckets();
        let mut by_row = by_row.finish()?;
        let mut named = KeySpill::<K>::create(&self.tmp, self.sort_bytes).map_err(store)?;
        let mut retired = croaring::Bitmap::new();
        for k in 0..buckets {
            let bytes = by_row.load(k)?;
            by_row.delete(k)?;
            for entry in bytes.chunks_exact(4 + K::WIDTH) {
                let row = u32::from_le_bytes(entry[..4].try_into().expect("four bytes"));
                // A row naming an item and giving it a value it did not hold: its old one goes.
                let Some(item) = numbers[row as usize].checked_sub(1) else {
                    continue;
                };
                let key = K::read(&entry[4..]);
                note(key);
                retired.add(item);
                named.push(key, item).map_err(store)?;
            }
        }
        drop(by_row);
        let _ = std::fs::remove_dir(&by_row_path);
        let named_path = self.scratch("held");
        let mut named_run = RunWriter::<K>::create(&named_path)?;
        named
            .drain(|key, item| {
                named_run
                    .push(key, item)
                    .map_err(|e| tessera_store::StoreError::MalformedBundle {
                        detail: e.to_string(),
                    })
            })
            .map_err(store)?;
        let runs = [created_run.finish()?, named_run.finish()?]
            .into_iter()
            .map(|receipt| HeldRun {
                receipt,
                base: None,
                source: None,
            })
            .collect();
        Ok((runs, retired, raised))
    }

    /// Drop the holdings of `items` from every run of one field: their values changed.
    fn retire(&mut self, position: u16, kind: KeyKind, items: &croaring::Bitmap) -> Result<()> {
        let runs = std::mem::take(self.held.entry(position).or_default());
        let mut kept = Vec::with_capacity(runs.len());
        for run in runs {
            let path = self.scratch("held");
            kept.push(match kind {
                KeyKind::Keyword => filter_run::<u128>(&run, &path, items)?,
                _ => filter_run::<u64>(&run, &path, items)?,
            });
            let _ = std::fs::remove_file(&run.receipt.path);
        }
        self.held.insert(position, kept);
        Ok(())
    }
}

/// Where a file's rows come from.
enum Input<'a> {
    File(&'a FileRead<'a>),
    Lists(&'a crate::layers::MemberLists, &'a Scanned),
}

/// What the rule lets a file's rows do, for the merge of one field.
#[derive(Clone, Copy)]
struct FieldContext {
    single: bool,
    creates: bool,
    sets_values: bool,
    one_row_per_item: bool,
}

fn push(updates: &mut Partition, row: u32, code: u32) -> Result<()> {
    let mut record = [0u8; 8];
    record[..4].copy_from_slice(&row.to_le_bytes());
    record[4..].copy_from_slice(&code.to_le_bytes());
    updates.push(&record).map_err(BuildError::from)
}

/// One held run being read: its reader, the base its rows are numbered from where it has one,
/// and the entry at its head.
type HeldStream<K> = (RunReader<K>, Option<u32>, Option<(K, u32)>);

/// The holdings of one field, read in key order to answer keys asked in ascending order.
struct HeldCursor<K: Key> {
    streams: Vec<HeldStream<K>>,
}

impl<K: Key> HeldCursor<K> {
    fn open(runs: &[HeldRun]) -> Result<Self> {
        let mut streams = Vec::with_capacity(runs.len());
        for run in runs {
            let mut reader = RunReader::open(&run.receipt)?;
            let head = reader.next_entry()?;
            streams.push((reader, run.base, head));
        }
        Ok(HeldCursor { streams })
    }

    /// The item holding `key`.
    fn holder(&mut self, key: K) -> Result<Option<u32>> {
        let mut found = None;
        for (reader, base, head) in &mut self.streams {
            while let Some((held, _)) = *head {
                if held >= key {
                    break;
                }
                *head = reader.next_entry()?;
            }
            if let Some((held, value)) = *head {
                if held == key {
                    found = Some(base.map_or(value, |base| base + value));
                }
            }
        }
        Ok(found)
    }

    /// Read every run to its end, which is where each is checked against its receipt.
    fn finish(mut self) -> Result<()> {
        for (reader, _, _) in &mut self.streams {
            while reader.next_entry()?.is_some() {}
        }
        Ok(())
    }
}

/// Where the merge of one field sends what it finds.
struct Merge<'a> {
    /// The field's place among those the file carries.
    index: usize,
    context: FieldContext,
    unset_path: &'a Path,
    updates: &'a mut Partition,
    /// The rows of each unset value several rows carry, where the file carries several fields
    /// and sets values.
    candidates: Option<&'a mut KeySpill<u64>>,
}

/// The run of one key being merged.
struct KeyRun<K> {
    key: K,
    holder: Option<u32>,
    /// Its first row, and how many rows it has had.
    first: u32,
    rows: u64,
    /// Its number among the field's candidates, once a second row makes it one.
    candidate: Option<u32>,
}

/// Merge one field's sorted `(key, row)` against its holdings: route each decision to `updates`,
/// write the keys nobody held for the holdings, and send the rows of each unset value several
/// rows carry to the candidates' sort. Returns what was found and how many decisions were routed.
fn merge_field<K: Key>(
    sort: KeySpill<K>,
    runs: &[HeldRun],
    merge: Merge<'_>,
) -> Result<(Merged, u64)> {
    let Merge {
        index,
        context,
        unset_path,
        updates,
        mut candidates,
    } = merge;
    let mut held = HeldCursor::<K>::open(runs)?;
    // The unset keys matter only where a row may set them: a new item's, or an edit's where the
    // file names items by another field too.
    let keeps_unset = context.sets_values && (context.creates || !context.single);
    let mut unset = match keeps_unset {
        true => Some(RunWriter::<K>::create(unset_path)?),
        false => None,
    };
    let mut numbered = 0u64;
    let mut decided = 0u64;
    let mut current: Option<KeyRun<K>> = None;
    let mut failure: Option<BuildError> = None;
    let store = |e: tessera_store::StoreError| BuildError::Invalid(e.to_string());
    sort.drain(|key, row| {
        let result = (|| -> Result<()> {
            if current.as_ref().is_none_or(|run| run.key != key) {
                current = Some(KeyRun {
                    key,
                    holder: held.holder(key)?,
                    first: row,
                    rows: 0,
                    candidate: None,
                });
            }
            let run = current.as_mut().expect("opened above");
            let first = run.rows == 0;
            run.rows += 1;
            match run.holder {
                Some(item) => {
                    let code = match first || !(context.single && context.one_row_per_item) {
                        true => item + 1,
                        false => ONE_ITEM_TWICE,
                    };
                    push(updates, row, code)?;
                    decided += 1;
                }
                None => {
                    if context.single && context.creates && !first {
                        push(updates, row, ONE_VALUE_TWICE)?;
                        decided += 1;
                    } else if let Some(unset) = unset.as_mut() {
                        unset.push(key, row)?;
                    }
                    if let Some(candidates) = candidates.as_deref_mut().filter(|_| !first) {
                        let number = match run.candidate {
                            Some(number) => number,
                            None => {
                                let number = numbered as u32;
                                numbered += 1;
                                let first = candidate_key(run.first, index);
                                candidates.push(first, number).map_err(store)?;
                                run.candidate = Some(number);
                                number
                            }
                        };
                        candidates
                            .push(candidate_key(row, index), number)
                            .map_err(store)?;
                    }
                }
            }
            Ok(())
        })();
        if let Err(e) = result {
            failure = Some(e);
            return Err(tessera_store::StoreError::MalformedBundle {
                detail: "the identity merge stopped".to_string(),
            });
        }
        Ok(())
    })
    .map_err(|e| failure.take().unwrap_or_else(|| store(e)))?;
    held.finish()?;
    Ok((
        Merged {
            unset: unset.map(RunWriter::finish).transpose()?,
            candidates: numbered,
        },
        decided,
    ))
}

/// Decide one row's claim on the unset values it carries that several rows carry, each a bit of
/// `claimed`: refused where an earlier row claimed one, and claiming them all otherwise. A row
/// refused, left out, or naming no item where rows do not create claims nothing.
fn claim(
    stored: &mut u32,
    row: u32,
    bits: &[u64],
    claimed: &mut [u64],
    creates: bool,
    tally: &mut Tally,
) {
    if *stored == 0 || *stored == LEFT_OUT || (*stored == PENDING && !creates) {
        return;
    }
    let set = |bit: u64| claimed[(bit / 64) as usize] & (1 << (bit % 64)) != 0;
    if bits.iter().any(|&bit| set(bit)) {
        *stored = 0;
        tally.refuse(Refusal::ONE_VALUE_TWICE, u64::from(row));
        return;
    }
    for &bit in bits {
        claimed[(bit / 64) as usize] |= 1 << (bit % 64);
    }
}

/// The rows a file creates, numbered as the walk decides them, and what a reader checks against.
struct FileNumbers {
    base: u64,
    creates: bool,
    created: Created,
    /// How many rows name an item, and the mixed sum of their numbers.
    anchor: (u64, u64),
}

impl FileNumbers {
    fn new(base: u64, creates: bool) -> FileNumbers {
        FileNumbers {
            base,
            creates,
            created: Created::default(),
            anchor: (0, 0),
        }
    }

    /// Decide a row the rule left pending, and count a row naming an item. A row `--limit` leaves
    /// out unless it names an item, and which names none, is left out, and reported where the
    /// file's rows do not create items.
    fn decide(&mut self, stored: &mut u32, row: u64, tally: &mut Tally) -> Result<()> {
        match *stored {
            PENDING if self.creates => {
                check_cap(self.base + self.created.count + 1)?;
                *stored = (self.base + self.created.count) as u32 + 1;
                self.created.mark(row);
            }
            PENDING => {
                *stored = 0;
                tally.refuse(Refusal::NAMES_NO_ITEM, row);
            }
            LEFT_OUT => {
                *stored = 0;
                if !self.creates {
                    tally.refuse(OUTSIDE_LIMIT, row);
                }
            }
            _ => {}
        }
        if let Some(number) = stored.checked_sub(1) {
            self.anchor.0 += 1;
            self.anchor.1 = self.anchor.1.wrapping_add(mix64(u64::from(number)));
        }
        Ok(())
    }
}

/// Which rows of a file created items, and how many before each: a created row's number is the
/// file's base plus the rows created before it.
#[derive(Default)]
struct Created {
    words: Vec<u64>,
    /// The created rows before each block of eight words.
    blocks: Vec<u64>,
    count: u64,
}

impl Created {
    fn mark(&mut self, row: u64) {
        let word = (row / 64) as usize;
        if self.words.len() <= word {
            self.words.resize(word + 1, 0);
        }
        self.words[word] |= 1 << (row % 64);
        self.count += 1;
    }

    fn seal(&mut self) {
        let mut before = 0u64;
        self.blocks = self
            .words
            .chunks(8)
            .map(|block| {
                let at = before;
                before += block.iter().map(|w| u64::from(w.count_ones())).sum::<u64>();
                at
            })
            .collect();
    }

    fn contains(&self, row: u64) -> bool {
        self.words
            .get((row / 64) as usize)
            .is_some_and(|w| w & (1 << (row % 64)) != 0)
    }

    /// The created rows before `row`.
    fn rank(&self, row: u64) -> u64 {
        let word = (row / 64) as usize;
        let block = word / 8;
        let mut rank = self.blocks[block];
        for w in &self.words[block * 8..word] {
            rank += u64::from(w.count_ones());
        }
        rank + u64::from((self.words[word] & ((1u64 << (row % 64)) - 1)).count_ones())
    }
}

/// Replay the routed decisions in row order into the file's numbers. Where the file carries one
/// field a row has at most one decision, so a bucket's are applied as they come and not sorted.
#[allow(clippy::too_many_arguments)]
fn walk(
    rows: &mut Rows,
    updates: &mut spill::PartitionStore,
    boundaries: &[u32],
    total: u64,
    lazy: bool,
    single: bool,
    tally: &mut Tally,
    named_items: &mut Option<KeySpill<u32>>,
    mut numbering: Option<&mut FileNumbers>,
) -> Result<()> {
    let numbers = rows.slice();
    let mut hits: Vec<(u32, u32)> = Vec::new();
    for (k, &lo) in boundaries.iter().enumerate() {
        let hi = boundaries
            .get(k + 1)
            .map_or(total, |&next| u64::from(next))
            .min(total);
        let bytes = updates.load(k)?;
        updates.delete(k)?;
        hits.clear();
        hits.extend(bytes.as_chunks::<8>().0.iter().map(|record| {
            (
                u32::from_le_bytes([record[0], record[1], record[2], record[3]]),
                u32::from_le_bytes([record[4], record[5], record[6], record[7]]),
            )
        }));
        drop(bytes);
        if single {
            if lazy {
                numbers[lo as usize..hi as usize].fill(PENDING);
            }
            for hit in &hits {
                let row = u64::from(hit.0);
                decide_row(
                    &mut numbers[row as usize],
                    row,
                    std::slice::from_ref(hit),
                    tally,
                    named_items,
                )?;
            }
            if let Some(numbering) = numbering.as_deref_mut() {
                for row in u64::from(lo)..hi {
                    numbering.decide(&mut numbers[row as usize], row, tally)?;
                }
            }
            continue;
        }
        hits.par_sort_unstable();
        let mut at = 0usize;
        for row in u64::from(lo)..hi {
            let stored = &mut numbers[row as usize];
            if lazy {
                *stored = PENDING;
            }
            let start = at;
            while at < hits.len() && u64::from(hits[at].0) == row {
                at += 1;
            }
            if start < at {
                decide_row(stored, row, &hits[start..at], tally, named_items)?;
            }
            if let Some(numbering) = numbering.as_deref_mut() {
                numbering.decide(stored, row, tally)?;
            }
        }
    }
    if let Some(numbering) = numbering {
        numbering.created.seal();
    }
    Ok(())
}

/// One row's decisions, sorted: a refusal where one was routed, and otherwise the item every
/// field named, or a refusal where they named two.
fn decide_row(
    stored: &mut u32,
    row: u64,
    decisions: &[(u32, u32)],
    tally: &mut Tally,
    named_items: &mut Option<KeySpill<u32>>,
) -> Result<()> {
    let codes = || decisions.iter().map(|&(_, code)| code);
    let refusal = [
        (UNKNOWN_TESSERA_ID, Refusal::UNKNOWN_TESSERA_ID),
        (ONE_ITEM_TWICE, Refusal::ONE_ITEM_TWICE),
        (ONE_VALUE_TWICE, Refusal::ONE_VALUE_TWICE),
    ]
    .into_iter()
    .find(|(code, _)| codes().any(|c| c == *code));
    // A row `--limit` leaves out unless it names an item, repeating a value no item holds, names
    // none: it is left out, and sets nothing.
    if refusal.is_some_and(|(code, _)| code == ONE_VALUE_TWICE) && *stored == LEFT_OUT {
        return Ok(());
    }
    if let Some((_, reason)) = refusal {
        *stored = 0;
        tally.refuse(reason, row);
        return Ok(());
    }
    let named = |code: u32| {
        (
            resolve::Identifier::Unique(0),
            EntityId::new(u64::from(code - 1)),
        )
    };
    let verdict = match decisions {
        [(_, code)] => Named::One(named(*code).1),
        _ => resolve::name_row(&codes().map(named).collect::<Vec<_>>()),
    };
    match verdict {
        Named::One(item) => {
            *stored = item.raw() as u32 + 1;
            if let Some(sort) = named_items {
                sort.push(item.raw() as u32, row as u32)
                    .map_err(|e| BuildError::Invalid(e.to_string()))?;
            }
        }
        Named::Two => {
            *stored = 0;
            tally.refuse(Refusal::NAMES_TWO, row);
        }
        Named::Nothing => {}
    }
    Ok(())
}

/// A copy of one held run without the entries of `items`.
fn filter_run<K: Key>(run: &HeldRun, path: &Path, items: &croaring::Bitmap) -> Result<HeldRun> {
    let mut reader = RunReader::<K>::open(&run.receipt)?;
    let mut writer = RunWriter::<K>::create(path)?;
    while let Some((key, value)) = reader.next_entry()? {
        let item = run.base.map_or(value, |base| base + value);
        if items.contains(item) {
            continue;
        }
        writer.push(key, value)?;
    }
    Ok(HeldRun {
        receipt: writer.finish()?,
        base: run.base,
        source: None,
    })
}
