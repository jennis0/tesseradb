//! Coalescing external-id runs: the half of a merge that is about **entity** space, reusable on
//! its own.
//!
//! A run is a file sorted by *caller-supplied* keys (contracts §2.4), and a lookup searches every
//! run whose own bounds admit the key — so the run count is a term in the ingest duplicate check
//! and in the drill-down, both of which a 90 s flush tick grows by ~960 a day. Bounding it is what
//! this module exists for, and it is deliberately separable from row space: the row-space merge
//! ([`crate::merge::execute_merge`]) calls [`merge_runs`] for its own runs, and the entity-space
//! coalesce publication calls it without touching a segment at all.
//!
//! **The merge is streaming**, holding *k* cursors rather than every pair, because compaction's
//! pass 3 runs this over the whole corpus's runs (compaction §3) and a merge may not carry a
//! `Vec` of every external id in its inputs.
//!
//! **Nothing here retires anything.** The keep-newest rule below drops an *older binding of the
//! same key*, which decision 0047 makes a forgotten, deleted holder — not a live entity's row, not
//! a posting, not an overlay entry. A function here that dropped a key because its entity was
//! deleted would be performing the compaction fold.

use std::cmp::Reverse;
use std::collections::BinaryHeap;
use std::fs::File;
use std::path::{Path, PathBuf};

use arrow::array::{Array, BinaryArray, UInt32Array};
use arrow::ipc::reader::FileReader;

use tessera_types::ROW_ABSENT;

use crate::error::{Result, StoreError};
use crate::flush::write_u32_array;
use crate::write::RunWriter;

/// Coalesce `runs` — prefix-relative order, **oldest first** — into one run and one locator
/// under `out_dir`, covering `[entity_lo, entity_hi]`. Returns the coalesced run's row count.
///
/// The entity-space publication's whole file-writing half, and **compaction's pass 3**
/// (compaction §3): a k-way merge over runs that are each already sorted by caller key, holding
/// *k* cursors rather than every pair.
///
/// **Oldest first is the keep-newest rule's only input.** A key present in two runs resolves to
/// the newest binding (decision 0047: delete plus re-ingest re-binds an external id, so the older
/// holder is a forgotten, deleted entity), and the reader resolves newest-run-first — so a caller
/// that reversed this would resurrect a forgotten holder and answer the wrong entity for a
/// re-ingested external id.
pub fn coalesce_external_id_runs(
    runs: &[PathBuf],
    entity_lo: u64,
    entity_hi: u64,
    out_dir: &Path,
) -> Result<usize> {
    merge_runs(&open_runs(runs)?, entity_lo, entity_hi, out_dir)
}

/// Open each run as a cursor, **oldest first**, holding only its mapped batches and a position.
pub(crate) fn open_runs(paths: &[PathBuf]) -> Result<Vec<RunCursor>> {
    paths.iter().map(|path| RunCursor::open(path)).collect()
}

/// A cursor over one already-sorted external-id run: `(external_id, entity)` ascending by the id
/// bytes, which is the order [`crate::flush::write_external_id_run`] writes and the sidecar
/// verifies at open.
pub(crate) struct RunCursor {
    batches: Vec<(BinaryArray, UInt32Array)>,
    batch: usize,
    row: usize,
}

impl RunCursor {
    fn open(path: &Path) -> Result<Self> {
        let file = File::open(path).map_err(|source| StoreError::Io {
            path: path.to_path_buf(),
            source,
        })?;
        let malformed = |e: arrow::error::ArrowError| StoreError::MalformedBundle {
            detail: format!("external-ids.arrow at {}: {e}", path.display()),
        };
        let reader = FileReader::try_new(file, None).map_err(malformed)?;
        let mut batches = Vec::new();
        for batch in reader {
            let batch = batch.map_err(malformed)?;
            let ids = batch
                .column(0)
                .as_any()
                .downcast_ref::<BinaryArray>()
                .ok_or_else(|| StoreError::MalformedBundle {
                    detail: "external-ids.arrow: column 0 is not binary".to_string(),
                })?
                .clone();
            let entities = batch
                .column(1)
                .as_any()
                .downcast_ref::<UInt32Array>()
                .ok_or_else(|| StoreError::MalformedBundle {
                    detail: "external-ids.arrow: column 1 is not uint32".to_string(),
                })?
                .clone();
            batches.push((ids, entities));
        }
        Ok(RunCursor {
            batches,
            batch: 0,
            row: 0,
        })
    }

    fn peek(&self) -> Option<(&[u8], u32)> {
        let (ids, entities) = self.batches.get(self.batch)?;
        Some((ids.value(self.row), entities.value(self.row)))
    }

    fn advance(&mut self) {
        self.row += 1;
        while let Some((ids, _)) = self.batches.get(self.batch) {
            if self.row < ids.len() {
                return;
            }
            self.batch += 1;
            self.row = 0;
        }
    }
}

/// Merge `cursors` (oldest first) into one run and one locator under `out_dir`, streaming.
///
/// **The output is exactly what a stable sort of the concatenation followed by a keep-last pass
/// produces**, which is what this replaced: ascending by key, ties broken by ascending run index
/// and then position, with only the last of each equal-key group emitted. That equivalence is the
/// whole correctness argument — see `coalesce_runs::a_key_in_two_runs_keeps_the_newest_binding`,
/// which is where the tie-break is pinned, and
/// `merge_execution::the_external_id_runs_coalesce_in_key_order` for the ordering.
///
/// **A heap that clones the key, rather than a linear scan or a borrow-free tournament.** A scan
/// over *k* per row is O(n·k), which is fine at a merge's `tier_width` of 4 and not at a fold's
/// live run count; a tournament over borrowed keys cannot be expressed without either unsafe or a
/// hand-rolled sift, since the comparison borrows the cursors the pop mutates. External ids are
/// capped at 64 bytes (contracts §1) and the fold is single-threaded and IO-throttled to
/// 128 MiB/s (compaction §6.1), so one small allocation per key buys O(n log k) and an obviously
/// correct pop. Memory stays O(k), which is the point.
pub(crate) fn merge_runs(
    cursors: &[RunCursor],
    entity_lo: u64,
    entity_hi: u64,
    out_dir: &Path,
) -> Result<usize> {
    let span =
        usize::try_from(entity_hi - entity_lo + 1).map_err(|_| StoreError::MalformedBundle {
            detail: format!("coalesce: entity span {entity_lo}..={entity_hi} is too wide"),
        })?;

    let mut cursors: Vec<RunCursor> = cursors
        .iter()
        .map(|c| RunCursor {
            batches: c.batches.clone(),
            batch: c.batch,
            row: c.row,
        })
        .collect();

    // `(key, run)` ascending — `Reverse` because `BinaryHeap` is a max-heap. The run index is the
    // tie-break and it is ascending, so an equal key from an older run is popped first and the
    // keep-last pass below then discards it in favour of the newer one.
    let mut heap: BinaryHeap<Reverse<(Vec<u8>, usize)>> = BinaryHeap::with_capacity(cursors.len());
    for (run, cursor) in cursors.iter().enumerate() {
        if let Some((key, _)) = cursor.peek() {
            heap.push(Reverse((key.to_vec(), run)));
        }
    }

    let io = |source| StoreError::Io {
        path: out_dir.join("external-ids.arrow"),
        source,
    };
    let mut writer = RunWriter::create(&out_dir.join("external-ids.arrow")).map_err(io)?;
    let mut locator = vec![ROW_ABSENT; span];
    let mut pending: Option<(Vec<u8>, u32)> = None;
    let mut rows = 0usize;

    let emit = |writer: &mut RunWriter,
                    locator: &mut Vec<u32>,
                    rows: &mut usize,
                    key: &[u8],
                    entity: u32|
     -> Result<()> {
        // **The locator's span is the caller's, never derived from the entities present** — an
        // entity ingested without an external id has no pair here at all, so a span taken from the
        // pairs would end short of it, and an entity past every locator extent but below the live
        // high-water is an *inconsistency* to the drill-down rather than an absent external id.
        let slot = (entity as u64).checked_sub(entity_lo).and_then(|i| {
            let i = usize::try_from(i).ok()?;
            (i < span).then_some(i)
        });
        let slot = slot.ok_or_else(|| StoreError::MalformedBundle {
            detail: format!(
                "coalesce: entity {entity} holds an external id but falls outside the locator \
                 span {entity_lo}..={entity_hi} — the reverse direction would have no home for it"
            ),
        })?;
        locator[slot] = *rows as u32;
        writer.append(key, entity).map_err(|source| StoreError::Io {
            path: out_dir.join("external-ids.arrow"),
            source,
        })?;
        *rows += 1;
        Ok(())
    };

    while let Some(Reverse((key, run))) = heap.pop() {
        let entity = cursors[run]
            .peek()
            .expect("a cursor is in the heap only while it has a pair")
            .1;
        cursors[run].advance();
        if let Some((next_key, _)) = cursors[run].peek() {
            heap.push(Reverse((next_key.to_vec(), run)));
        }

        // Keep-last, with one item of lookahead: the pending pair survives only if the pair that
        // follows it carries a different key. A newer binding of the same key supersedes it.
        if let Some((prev_key, prev_entity)) = pending.take() {
            if prev_key != key {
                emit(&mut writer, &mut locator, &mut rows, &prev_key, prev_entity)?;
            }
        }
        pending = Some((key, entity));
    }
    if let Some((key, entity)) = pending {
        emit(&mut writer, &mut locator, &mut rows, &key, entity)?;
    }

    let written = writer.finish().map_err(io)?;
    debug_assert_eq!(written, rows);
    write_u32_array(&out_dir.join("ext-locator.u32"), &locator)?;
    Ok(rows)
}
