//! Lookups across a set of runs.

use std::path::Path;
use std::sync::Arc;

use super::{Key, KeyRun};
use crate::error::{Result, StoreError};

/// Which run of a [`KeyIndexView`] an entry was found in: a position in the live list (newest
/// first) or in the base list.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum RunRef {
    Live(usize),
    Base(usize),
}

/// One entity stored under a key, and the run it is stored in.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Found {
    pub run: RunRef,
    pub entity: u32,
}

/// The runs of one index: live runs, newest first, whose key ranges may overlap anything; and
/// base runs in ascending key order with disjoint key ranges. A lookup reads each live run whose
/// range holds the key and the one base run whose range holds it.
pub struct KeyIndexView<K: Key> {
    live: Vec<Arc<KeyRun<K>>>,
    base: Vec<Arc<KeyRun<K>>>,
    /// The non-empty base runs' key ranges, ascending, with their positions in `base`.
    base_ranges: Vec<(K, K, usize)>,
}

impl<K: Key> KeyIndexView<K> {
    /// Refused when the non-empty base runs are not in ascending key order with disjoint ranges.
    pub fn new(live: Vec<Arc<KeyRun<K>>>, base: Vec<Arc<KeyRun<K>>>) -> Result<Self> {
        let base_ranges: Vec<(K, K, usize)> = base
            .iter()
            .enumerate()
            .filter_map(|(i, run)| run.key_range().map(|(lo, hi)| (lo, hi, i)))
            .collect();
        if let Some(w) = base_ranges.windows(2).find(|w| w[0].1 >= w[1].0) {
            return Err(StoreError::MalformedBundle {
                detail: format!(
                    "base key runs {} and {} overlap or are out of order; list base runs in \
                     ascending key order with disjoint ranges, and list any other run as live",
                    display(base[w[0].2].path()),
                    display(base[w[1].2].path()),
                ),
            });
        }
        Ok(KeyIndexView {
            live,
            base,
            base_ranges,
        })
    }

    pub fn live(&self) -> &[Arc<KeyRun<K>>] {
        &self.live
    }

    pub fn base(&self) -> &[Arc<KeyRun<K>>] {
        &self.base
    }

    /// The base run whose range holds `key`, if one does.
    fn base_for(&self, key: K) -> Option<usize> {
        let i = self.base_ranges.partition_point(|&(lo, _, _)| lo <= key);
        let &(_, hi, run) = self.base_ranges.get(i.checked_sub(1)?)?;
        (key <= hi).then_some(run)
    }

    /// Every entity stored under `key` in any run: the live runs' in live order, newest first,
    /// then the base run's.
    pub fn get(&self, key: K) -> Result<Vec<Found>> {
        let mut out = Vec::new();
        for (i, run) in self.live.iter().enumerate() {
            for entity in run.get(key)? {
                out.push(Found {
                    run: RunRef::Live(i),
                    entity,
                });
            }
        }
        if let Some(i) = self.base_for(key) {
            for entity in self.base[i].get(key)? {
                out.push(Found {
                    run: RunRef::Base(i),
                    entity,
                });
            }
        }
        Ok(out)
    }

    /// [`KeyIndexView::get`] for each of `keys`, which must be in ascending order, as
    /// `(position in keys, found)` ordered by position and then as `get` orders them. Each run is
    /// walked at most once.
    ///
    /// # Panics
    ///
    /// If `keys` is not in ascending order.
    pub fn lookup_sorted(&self, keys: &[K]) -> Result<Vec<(usize, Found)>> {
        assert!(
            keys.windows(2).all(|w| w[0] <= w[1]),
            "lookup_sorted takes keys in ascending order"
        );
        let mut out = Vec::new();
        for (r, run) in self.live.iter().enumerate() {
            for (i, entity) in run.lookup_sorted(keys)? {
                out.push((
                    i,
                    Found {
                        run: RunRef::Live(r),
                        entity,
                    },
                ));
            }
        }
        for &(lo, hi, r) in &self.base_ranges {
            let from = keys.partition_point(|&k| k < lo);
            let to = keys.partition_point(|&k| k <= hi);
            if from >= to {
                continue;
            }
            for (i, entity) in self.base[r].lookup_sorted(&keys[from..to])? {
                out.push((
                    from + i,
                    Found {
                        run: RunRef::Base(r),
                        entity,
                    },
                ));
            }
        }
        // Stable: within one key and one run, entities keep the run's ascending order.
        out.sort_by_key(|&(i, found)| (i, found.run));
        Ok(out)
    }
}

fn display(path: &Path) -> String {
    path.display().to_string()
}
