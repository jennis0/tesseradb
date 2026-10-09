//! The denied-row labels held in memory, per `(view, layer, level)`, under a byte bound: the two
//! newest versions of each, the least recently used level's dropped first when the bound is
//! passed, and each written to disk with at most one write under way per level.

use std::collections::VecDeque;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex, PoisonError};

use rustc_hash::FxHashMap;

use super::cache::LevelAddress;
use super::denied::DeniedLabels;

/// How many versions of one level's labels are kept, for a request still reading a form from before
/// a publication.
const VERSIONS_KEPT: usize = 2;

/// The bytes of denied-row labels held. A denied row costs about 24 B a level, so this holds about
/// ten million denied rows across the levels read.
pub(crate) const DEFAULT_LABELS_BYTES: u64 = 256 << 20;

/// One level's labels.
#[derive(Default)]
struct Slot {
    versions: VecDeque<Arc<DeniedLabels>>,
    /// When it was last read or written, on [`HeldLabels`]' clock.
    used: u64,
    /// Whether a write of this level's labels is queued or under way.
    writing: bool,
    /// The newest version written to disk.
    written: Option<u64>,
}

#[derive(Default)]
struct Held {
    slots: FxHashMap<LevelAddress, Slot>,
    bytes: u64,
    clock: u64,
}

/// The labels held, and where and under what bound they are written: what a request and the
/// worker both bring labels forward through.
#[derive(Clone)]
pub(crate) struct LabelStore {
    pub(crate) held: Arc<HeldLabels>,
    pub(crate) dir: Option<std::path::PathBuf>,
    pub(crate) disk_bound: Arc<AtomicU64>,
}

/// See the module doc.
pub(crate) struct HeldLabels {
    held: Mutex<Held>,
    bound: AtomicU64,
    rows_read: AtomicU64,
}

/// What [`HeldLabels`] reports.
pub(crate) struct LabelStats {
    pub(crate) bytes: u64,
    pub(crate) bound: u64,
    pub(crate) rows_read: u64,
}

impl HeldLabels {
    pub(crate) fn new(bound: u64) -> Self {
        HeldLabels {
            held: Mutex::default(),
            bound: AtomicU64::new(bound),
            rows_read: AtomicU64::new(0),
        }
    }

    pub(crate) fn stats(&self) -> LabelStats {
        LabelStats {
            bytes: self
                .held
                .lock()
                .unwrap_or_else(PoisonError::into_inner)
                .bytes,
            bound: self.bound.load(Ordering::Relaxed),
            rows_read: self.rows_read.load(Ordering::Relaxed),
        }
    }

    /// Count `rows` denied rows read from a column.
    pub(crate) fn read(&self, rows: u64) {
        self.rows_read.fetch_add(rows, Ordering::Relaxed);
    }

    /// Every version held for `address`, oldest first.
    pub(crate) fn versions(&self, address: &LevelAddress) -> Vec<Arc<DeniedLabels>> {
        let mut held = self.held.lock().unwrap_or_else(PoisonError::into_inner);
        held.clock += 1;
        let clock = held.clock;
        match held.slots.get_mut(address) {
            Some(slot) => {
                slot.used = clock;
                slot.versions.iter().cloned().collect()
            }
            None => Vec::new(),
        }
    }

    /// The addresses held.
    pub(crate) fn addresses(&self) -> Vec<LevelAddress> {
        let held = self.held.lock().unwrap_or_else(PoisonError::into_inner);
        held.slots.keys().cloned().collect()
    }

    /// Keep `labels` among `address`'s versions, dropping any of another column or bundle, and
    /// the least recently used levels while the bound is passed. `true` where a write of this
    /// level should be queued: none is under way and this version is newer than the one written.
    pub(crate) fn hold(&self, address: LevelAddress, labels: Arc<DeniedLabels>) -> bool {
        let mut held = self.held.lock().unwrap_or_else(PoisonError::into_inner);
        held.clock += 1;
        let clock = held.clock;
        let slot = held.slots.entry(address.clone()).or_default();
        let before: u64 = slot.versions.iter().map(|v| v.weight_bytes()).sum();
        slot.versions.retain(|v| {
            v.identity == labels.identity && v.column == labels.column && v.at != labels.at
        });
        let at = slot.versions.partition_point(|v| v.at < labels.at);
        slot.versions.insert(at, labels);
        while slot.versions.len() > VERSIONS_KEPT {
            slot.versions.pop_front();
        }
        slot.used = clock;
        let after: u64 = slot.versions.iter().map(|v| v.weight_bytes()).sum();
        let newest = slot.versions.back().map_or(0, |v| v.at);
        let write = !slot.writing && slot.written.is_none_or(|written| written < newest);
        if write {
            slot.writing = true;
        }
        held.bytes = held.bytes + after - before;
        let bound = self.bound.load(Ordering::Relaxed);
        while held.bytes > bound {
            let Some(coldest) = held
                .slots
                .iter()
                .filter(|(a, _)| **a != address)
                .min_by_key(|(_, slot)| slot.used)
                .map(|(a, _)| a.clone())
            else {
                break;
            };
            if let Some(slot) = held.slots.remove(&coldest) {
                held.bytes -= slot.versions.iter().map(|v| v.weight_bytes()).sum::<u64>();
            }
        }
        write
    }

    /// The newest version of `address` to write, ending the write under way where it is written
    /// already, so at most one write per level is queued.
    pub(crate) fn to_write(&self, address: &LevelAddress) -> Option<Arc<DeniedLabels>> {
        let mut held = self.held.lock().unwrap_or_else(PoisonError::into_inner);
        let slot = held.slots.get_mut(address)?;
        let newest = slot.versions.back().cloned();
        match newest {
            Some(newest) if slot.written.is_none_or(|written| written < newest.at) => Some(newest),
            _ => {
                slot.writing = false;
                None
            }
        }
    }

    /// Record that `at` is on disk for `address`.
    pub(crate) fn wrote(&self, address: &LevelAddress, at: u64) {
        let mut held = self.held.lock().unwrap_or_else(PoisonError::into_inner);
        if let Some(slot) = held.slots.get_mut(address) {
            slot.written = Some(slot.written.map_or(at, |w| w.max(at)));
        }
    }
}
