//! Merging runs into new runs.

use std::cmp::Reverse;
use std::collections::BinaryHeap;
use std::path::{Path, PathBuf};

use croaring::Bitmap;

use super::run::Entries;
use super::{Key, KeyRun, KeyRunWriter, WrittenRun};
use crate::error::Result;

/// Merge the runs at `inputs` into new runs under `out_dir` (named as [`KeyRunWriter`] names
/// them), dropping every entry whose entity is in `retired` and keeping every other entry once.
/// An empty `retired` merges without dropping anything.
///
/// The output is split as [`KeyRunWriter`] splits, so the runs it returns have disjoint key ranges
/// whatever the inputs' ranges were. Each input is read front to back once, every page checked as
/// it is reached; memory is one page cursor per input.
pub fn merge_runs<K: Key>(
    inputs: &[PathBuf],
    retired: &Bitmap,
    out_dir: &Path,
    stem: &str,
    max_entries: u64,
) -> Result<Vec<WrittenRun<K>>> {
    let runs = inputs
        .iter()
        .map(|path| KeyRun::<K>::open_sequential(path))
        .collect::<Result<Vec<_>>>()?;
    let mut cursors: Vec<Entries<'_, K>> = runs.iter().map(KeyRun::iter).collect();
    let mut heap = BinaryHeap::with_capacity(cursors.len());
    for (i, cursor) in cursors.iter_mut().enumerate() {
        if let Some(entry) = cursor.next() {
            let (key, entity) = entry?;
            heap.push(Reverse((key, entity, i)));
        }
    }

    let mut writer = KeyRunWriter::create(out_dir, stem, max_entries);
    let mut last: Option<(K, u32)> = None;
    while let Some(Reverse((key, entity, i))) = heap.pop() {
        if let Some(entry) = cursors[i].next() {
            let (next_key, next_entity) = entry?;
            heap.push(Reverse((next_key, next_entity, i)));
        }
        if last == Some((key, entity)) || retired.contains(entity) {
            continue;
        }
        writer.push(key, entity)?;
        last = Some((key, entity));
    }
    writer.finish()
}
