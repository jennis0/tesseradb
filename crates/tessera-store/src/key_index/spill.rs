//! Sorting entries that arrive in any order into runs, under a memory budget.
//!
//! Entries are held in memory until they pass half the budget. Past that, they are routed to 256
//! bucket files by the top eight bits of the key, and each bucket is then read back, sorted and
//! written out in key order. A bucket too large to sort in the budget is routed again, by the
//! eight bits below the bits every entry in it shares (its smallest and largest entries are
//! tracked as it is written), so sequential and clustered keys cost one more pass and never more
//! memory. A bucket holding one key is routed by entity in the same way.

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
        self.held.push((key, entity));
        if self.held.len() >= (self.budget / 2 / held_bytes::<K>()).max(1) {
            let root = Split::Key(8 * K::WIDTH as u32 - 8);
            let mut level = self.level(root)?;
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
    /// entity, with the number of entities.
    pub fn finish(
        mut self,
        out_dir: &Path,
        stem: &str,
        max_entries: u64,
        on_duplicate: impl FnMut(K, u64),
    ) -> Result<Vec<WrittenRun<K>>> {
        let mut sink = Sink {
            writer: KeyRunWriter::create(out_dir, stem, max_entries),
            last: None,
            group: None,
            on_duplicate,
        };
        match self.spilled.take() {
            None => {
                let mut held = std::mem::take(&mut self.held);
                held.sort_unstable();
                for (key, entity) in held {
                    sink.push(key, entity)?;
                }
            }
            Some(level) => {
                let (store, bounds) = level.finish()?;
                self.drain(store, bounds, &mut sink)?;
            }
        }
        sink.finish()
    }

    /// Write out each bucket of one level in key order: sorted in memory when it fits the budget,
    /// routed into a finer level when it does not.
    fn drain<F: FnMut(K, u64)>(
        &mut self,
        mut store: PartitionStore,
        bounds: Vec<Option<Bounds<K>>>,
        sink: &mut Sink<K, F>,
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
                self.drain(finer_store, finer_bounds, sink)?;
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

/// Which eight bits of an entry pick its bucket: those at `shift` and up in the key, or in the
/// entity.
#[derive(Debug, Clone, Copy)]
enum Split {
    Key(u32),
    Entity(u32),
}

impl Split {
    fn digit<K: Key>(self, key: K, entity: u32) -> usize {
        match self {
            Split::Key(shift) => ((key.widen() >> shift) & 0xFF) as usize,
            Split::Entity(shift) => ((entity >> shift) & 0xFF) as usize,
        }
    }

    /// The split for entries between `min` and `max`: the eight bits from the highest bit in
    /// which they differ down. Every entry between them shares the bits above it, so the bucket
    /// order is the entry order and `min` and `max` land in different buckets. `None` when
    /// `min == max`.
    fn between<K: Key>(min: (K, u32), max: (K, u32)) -> Option<Split> {
        if min.0 != max.0 {
            let top = 128 - (min.0.widen() ^ max.0.widen()).leading_zeros();
            Some(Split::Key(top.saturating_sub(8)))
        } else if min.1 != max.1 {
            let top = 32 - (min.1 ^ max.1).leading_zeros();
            Some(Split::Entity(top.saturating_sub(8)))
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

/// The sorted stream's end: drops exact repeats, counts each key's entities, writes.
struct Sink<K: Key, F: FnMut(K, u64)> {
    writer: KeyRunWriter<K>,
    last: Option<(K, u32)>,
    group: Option<(K, u64)>,
    on_duplicate: F,
}

impl<K: Key, F: FnMut(K, u64)> Sink<K, F> {
    fn push(&mut self, key: K, entity: u32) -> Result<()> {
        if self.last == Some((key, entity)) {
            return Ok(());
        }
        match &mut self.group {
            Some((k, n)) if *k == key => *n += 1,
            _ => {
                self.close_group();
                self.group = Some((key, 1));
            }
        }
        self.writer.push(key, entity)?;
        self.last = Some((key, entity));
        Ok(())
    }

    fn close_group(&mut self) {
        if let Some((key, n)) = self.group.take() {
            if n > 1 {
                (self.on_duplicate)(key, n);
            }
        }
    }

    fn finish(mut self) -> Result<Vec<WrittenRun<K>>> {
        self.close_group();
        self.writer.finish()
    }
}
