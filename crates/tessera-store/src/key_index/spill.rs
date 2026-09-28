//! Sorting entries that arrive in any order into runs, under a memory budget.
//!
//! Entries are held in memory until they pass half the budget. Past that, they are routed to 256
//! bucket files over the range of keys held so far, and each bucket is then read back, sorted and
//! written out in key order. A bucket too large to sort in the budget is routed again over its own
//! range (its smallest and largest entries are tracked as it is written), so clustered keys cost
//! one more pass and never more memory. A bucket holding one key is routed by entity in the same
//! way.

use std::num::NonZeroU64;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};

use super::{Key, KeyRunWriter, WrittenRun};
use crate::error::{Result, StoreError};
use crate::partition::{Partition, PartitionStore};

const FANOUT: usize = 256;

/// Sorts `(key, entity)` entries pushed in any order into runs with disjoint key ranges, holding
/// about `memory_budget` bytes at most, plus 256 bucket buffers of at least a page each once it
/// spills. Exact repeats of an entry are kept once.
pub struct KeySpill<K: Key> {
    scratch: PathBuf,
    budget: usize,
    held: Vec<(K, u32)>,
    spilled: Option<Level<K>>,
    levels: u64,
}

/// A key held by more than one entity, as [`KeySpill::finish`] reports it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct DuplicateKey<K: Key> {
    pub key: K,
    /// The key's two smallest entities.
    pub first: [u32; 2],
    /// How many entities hold the key.
    pub entities: u64,
}

/// The bytes one held entry costs.
const fn held_bytes<K: Key>() -> usize {
    std::mem::size_of::<(K, u32)>()
}

impl<K: Key> KeySpill<K> {
    /// A spill whose bucket files go in a new directory under `scratch`, removed when the spill
    /// is dropped.
    pub fn create(scratch: &Path, memory_budget: usize) -> Result<Self> {
        static NEXT: AtomicU64 = AtomicU64::new(0);
        let dir = scratch.join(format!(
            "keyspill-{}-{}",
            std::process::id(),
            NEXT.fetch_add(1, Ordering::Relaxed)
        ));
        std::fs::create_dir_all(&dir).map_err(|source| StoreError::Io {
            path: dir.clone(),
            source,
        })?;
        Ok(KeySpill {
            scratch: dir,
            budget: memory_budget,
            held: Vec::new(),
            spilled: None,
            levels: 0,
        })
    }

    pub fn push(&mut self, key: K, entity: u32) -> Result<()> {
        if let Some(level) = &mut self.spilled {
            return level.push(key, entity);
        }
        let threshold = (self.budget / 2 / held_bytes::<K>()).max(1);
        if self.held.capacity() == 0 {
            self.held.reserve_exact(threshold);
        }
        self.held.push((key, entity));
        if self.held.len() >= threshold {
            let mut level = self.level(Split::root(&self.held))?;
            for &(key, entity) in &self.held {
                level.push(key, entity)?;
            }
            self.held = Vec::new();
            self.spilled = Some(level);
        }
        Ok(())
    }

    fn level(&mut self, split: Split) -> Result<Level<K>> {
        self.levels += 1;
        Level::create(
            &self.scratch,
            &format!("level-{}", self.levels),
            split,
            self.budget,
        )
    }

    /// Write every entry, sorted, as runs under `out_dir` (named and split as [`KeyRunWriter`]
    /// names and splits them), and call `on_duplicate` once for each key held by more than one
    /// entity.
    pub fn finish(
        mut self,
        out_dir: &Path,
        stem: &str,
        max_entries: NonZeroU64,
        on_duplicate: impl FnMut(DuplicateKey<K>),
    ) -> Result<Vec<WrittenRun<K>>> {
        let mut sink = Sink {
            writer: KeyRunWriter::create(out_dir, stem, max_entries),
            last: None,
            group: None,
            on_duplicate,
        };
        self.sort_into(&mut sink)?;
        sink.finish()
    }

    /// Hand every entry to `visit` in `(key, entity)` order, each once, rather than writing runs:
    /// for a caller that merges the sorted entries against something of its own.
    pub fn drain(mut self, visit: impl FnMut(K, u32) -> Result<()>) -> Result<()> {
        let mut out = Visit { last: None, visit };
        self.sort_into(&mut out)
    }

    fn sort_into(&mut self, out: &mut impl Out<K>) -> Result<()> {
        match self.spilled.take() {
            None => {
                let mut held = std::mem::take(&mut self.held);
                held.sort_unstable();
                for (key, entity) in held {
                    out.push(key, entity)?;
                }
            }
            Some(level) => {
                let (store, bounds) = level.finish()?;
                self.drain_level(store, bounds, out)?;
            }
        }
        Ok(())
    }

    /// Write out each bucket of one level in key order: sorted in memory when it fits the budget,
    /// routed into a finer level when it does not.
    fn drain_level(
        &mut self,
        mut store: PartitionStore,
        bounds: Vec<Option<Bounds<K>>>,
        sink: &mut impl Out<K>,
    ) -> Result<()> {
        let width = K::WIDTH + 4;
        for (k, b) in bounds.into_iter().enumerate() {
            let Some(b) = b else { continue };
            let fits = b.count.saturating_mul((width + held_bytes::<K>()) as u64)
                <= (self.budget / 2) as u64;
            if fits {
                let bytes = store.load(k)?;
                store.delete(k)?;
                let mut entries: Vec<(K, u32)> = bytes.chunks_exact(width).map(decode).collect();
                drop(bytes);
                entries.sort_unstable();
                for (key, entity) in entries {
                    sink.push(key, entity)?;
                }
            } else if let Some(split) = Split::between(b.min, b.max) {
                let mut finer = self.level(split)?;
                store.read_each(k, |record| {
                    let (key, entity) = decode(record);
                    finer.push(key, entity)
                })?;
                store.delete(k)?;
                let (finer_store, finer_bounds) = finer.finish()?;
                self.drain_level(finer_store, finer_bounds, sink)?;
            } else {
                // Every entry in the bucket is the same entry; check the file before using it.
                store.read_each(k, |_| Ok(()))?;
                store.delete(k)?;
                sink.push(b.min.0, b.min.1)?;
            }
        }
        Ok(())
    }
}

impl<K: Key> Drop for KeySpill<K> {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.scratch);
    }
}

fn decode<K: Key>(record: &[u8]) -> (K, u32) {
    let entity = u32::from_le_bytes(record[K::WIDTH..K::WIDTH + 4].try_into().expect("four"));
    (K::read(record), entity)
}

/// Which bucket an entry goes to: its key, or its entity, less `base` and shifted right by
/// `shift`, with anything past the last bucket in the last. The bucket is monotone in the entry, so
/// the bucket order is the entry order.
#[derive(Debug, Clone, Copy)]
enum Split {
    Key { base: u128, shift: u32 },
    Entity { base: u32, shift: u32 },
}

impl Split {
    fn digit<K: Key>(self, key: K, entity: u32) -> usize {
        let d = match self {
            Split::Key { base, shift } => key.widen().saturating_sub(base) >> shift,
            Split::Entity { base, shift } => u128::from(entity.saturating_sub(base) >> shift),
        };
        d.min(FANOUT as u128 - 1) as usize
    }

    /// The split for the first entries held, spread over their own range of keys rather than
    /// the key type's: keys that are small integers share their top bits, and routing them by
    /// those would put every entry in one bucket and write it all a second time. A later entry
    /// outside the range goes to the first or last bucket, which is routed again if it grows too
    /// large. So input that arrives sorted, whose first entries hold only the lowest keys, sends
    /// nearly everything after them to the last bucket and pays that second pass; the spill is
    /// for input in any order, and sorted input can go to a run writer directly.
    fn root<K: Key>(held: &[(K, u32)]) -> Split {
        let min = held.iter().map(|e| e.0).min();
        let max = held.iter().map(|e| e.0).max();
        match (min, max) {
            (Some(min), Some(max)) if min != max => Split::keys(min, max),
            _ => Split::Key {
                base: 0,
                shift: 8 * K::WIDTH as u32 - 8,
            },
        }
    }

    /// Keys from `min` to `max`, `min < max`, spread over the buckets.
    fn keys<K: Key>(min: K, max: K) -> Split {
        let span = max.widen() - min.widen();
        Split::Key {
            base: min.widen(),
            shift: (128 - span.leading_zeros()).saturating_sub(8),
        }
    }

    /// The split for entries between `min` and `max`: by key where the keys differ, else by
    /// entity. `None` when `min == max`.
    fn between<K: Key>(min: (K, u32), max: (K, u32)) -> Option<Split> {
        if min.0 != max.0 {
            Some(Split::keys(min.0, max.0))
        } else if min.1 != max.1 {
            let span = max.1 - min.1;
            Some(Split::Entity {
                base: min.1,
                shift: (32 - span.leading_zeros()).saturating_sub(8),
            })
        } else {
            None
        }
    }
}

/// How many entries a bucket holds and its smallest and largest.
#[derive(Debug, Clone, Copy)]
struct Bounds<K: Key> {
    count: u64,
    min: (K, u32),
    max: (K, u32),
}

/// 256 bucket files being written, one split's worth.
struct Level<K: Key> {
    partition: Partition,
    split: Split,
    bounds: Vec<Option<Bounds<K>>>,
    record: Vec<u8>,
}

impl<K: Key> Level<K> {
    fn create(dir: &Path, name: &str, split: Split, budget: usize) -> Result<Self> {
        let width = K::WIDTH + 4;
        // Sized so the 256 buffers take about a quarter of the budget, within the partition's
        // own floor and ceiling.
        let records = (budget / 4 / width) as u64;
        Ok(Level {
            partition: Partition::create_routed(dir, name, FANOUT, width, records)?,
            split,
            bounds: vec![None; FANOUT],
            record: vec![0u8; width],
        })
    }

    fn push(&mut self, key: K, entity: u32) -> Result<()> {
        let d = self.split.digit(key, entity);
        key.write(&mut self.record);
        self.record[K::WIDTH..].copy_from_slice(&entity.to_le_bytes());
        self.partition.push_to(d, &self.record)?;
        let entry = (key, entity);
        let b = self.bounds[d].get_or_insert(Bounds {
            count: 0,
            min: entry,
            max: entry,
        });
        b.count += 1;
        b.min = b.min.min(entry);
        b.max = b.max.max(entry);
        Ok(())
    }

    fn finish(self) -> Result<(PartitionStore, Vec<Option<Bounds<K>>>)> {
        Ok((self.partition.finish()?, self.bounds))
    }
}

/// Where the sorted stream goes.
trait Out<K: Key> {
    fn push(&mut self, key: K, entity: u32) -> Result<()>;
}

/// The sorted stream handed to a caller, exact repeats dropped.
struct Visit<K: Key, F: FnMut(K, u32) -> Result<()>> {
    last: Option<(K, u32)>,
    visit: F,
}

impl<K: Key, F: FnMut(K, u32) -> Result<()>> Out<K> for Visit<K, F> {
    fn push(&mut self, key: K, entity: u32) -> Result<()> {
        if self.last == Some((key, entity)) {
            return Ok(());
        }
        self.last = Some((key, entity));
        (self.visit)(key, entity)
    }
}

/// The sorted stream's end: drops exact repeats, gathers each key's entities, writes.
struct Sink<K: Key, F: FnMut(DuplicateKey<K>)> {
    writer: KeyRunWriter<K>,
    last: Option<(K, u32)>,
    group: Option<DuplicateKey<K>>,
    on_duplicate: F,
}

impl<K: Key, F: FnMut(DuplicateKey<K>)> Out<K> for Sink<K, F> {
    fn push(&mut self, key: K, entity: u32) -> Result<()> {
        if self.last == Some((key, entity)) {
            return Ok(());
        }
        match &mut self.group {
            Some(group) if group.key == key => {
                if group.entities == 1 {
                    group.first[1] = entity;
                }
                group.entities += 1;
            }
            _ => {
                self.close_group();
                self.group = Some(DuplicateKey {
                    key,
                    first: [entity, entity],
                    entities: 1,
                });
            }
        }
        self.writer.push(key, entity)?;
        self.last = Some((key, entity));
        Ok(())
    }
}

impl<K: Key, F: FnMut(DuplicateKey<K>)> Sink<K, F> {
    fn close_group(&mut self) {
        if let Some(group) = self.group.take() {
            if group.entities > 1 {
                (self.on_duplicate)(group);
            }
        }
    }

    fn finish(mut self) -> Result<Vec<WrittenRun<K>>> {
        self.close_group();
        self.writer.finish()
    }
}
