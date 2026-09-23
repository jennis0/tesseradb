//! Coalescing external-id runs: the half of a merge that is about **entity** space, reusable on
//! its own — and, at the fold's scale, compaction's pass 3 (compaction §3).
//!
//! A run is a file sorted by *caller-supplied* keys (contracts §2.4), and a lookup searches every
//! run whose own bounds admit the key — so the run count is a term in the ingest duplicate check
//! and in the drill-down, both of which a 90 s flush tick grows by ~960 a day. Bounding it is what
//! this module exists for, and it is deliberately separable from row space: the row-space merge
//! ([`crate::merge::execute_merge`]) calls [`merge_runs`] for its own runs, and the entity-space
//! coalesce publication calls it without touching a segment at all.
//!
//! **The merge is streaming**, holding *k* cursors rather than every pair, because compaction's
//! pass 3 runs this over the whole corpus's runs and a merge may not carry a `Vec` of every
//! external id in its inputs.
//!
//! **One merge, two entry points, one implementation.** [`merge_runs`] (via
//! [`coalesce_external_id_runs`]) is the ordinary maintenance coalesce: nothing is dropped, and
//! the locator is a small in-memory `Vec`, because the span a merge ever covers is bounded by the
//! run-count policy this module exists to enforce. [`fold_external_id_runs`] is the **same**
//! keep-newest merge — `merge_runs_core` beneath both is the one implementation, never two — with
//! the two things a merge's span never needs: dropping the keys of the entities `tombstones`
//! names, and writing the locator through [`crate::locator::LocatorWriter`]'s mapping rather than
//! a `Vec`, because the fold's span is the whole entity space (compaction §3: 4 GB resident at
//! 10⁹ as a `Vec`, page cache through a mapping).
//!
//! **Dropping a key here is not Rule F's retirement, and does not claim to be.** Rule F's route
//! out of `Overlay::deleted` is `Overlay::retire` (`tessera-lifecycle`, compaction §5) — a
//! different store in a different crate. What this module owns is the artefact half compaction
//! §3's pass 3 requires: an entity named in `tombstones` leaves run 0, and its locator slot reads
//! [`tessera_types::ROW_ABSENT`] rather than a live ordinal — one of the artefacts `executed`'s
//! derivation (compaction §5) checks an entity's absence from. Doing this is necessary — a
//! binding left standing turns a lawful re-ingest of that external id into a 409, because the
//! engine's duplicate check exempts a holder only while `overlay.is_deleted` is true
//! (`tessera-engine`'s `established_collisions`), and retirement makes that false (decision 0047)
//! — and it is **not** sufficient alone: the live `established` map the engine holds in memory
//! must be cleared at the same swap, or that path still answers with the dropped key. That half is
//! the engine's, not this module's.

use std::cmp::Reverse;
use std::collections::BinaryHeap;
use std::fs::File;
use std::path::{Path, PathBuf};
use std::ptr::NonNull;
use std::sync::Arc;

use arrow::array::{Array, BinaryArray, UInt32Array};
use arrow::buffer::Buffer;
use croaring::Bitmap;
use memmap2::Mmap;

use tessera_types::ROW_ABSENT;

use crate::error::{Result, StoreError};
use crate::flush::write_u32_array;
use crate::locator::LocatorWriter;
use crate::read::decode_single_batch;
use crate::write::RunWriter;

/// Coalesce `runs` — prefix-relative order, **oldest first** — into one run and one locator
/// under `out_dir`, covering `[entity_lo, entity_hi]`. Returns the coalesced run's row count.
///
/// The entity-space publication's whole file-writing half — the **ordinary maintenance** entry
/// point; see [`fold_external_id_runs`] for the fold's own, over the whole corpus rather than one
/// maintenance window (compaction §3, pass 3): a k-way merge over runs that are each already
/// sorted by caller key, holding *k* cursors rather than every pair.
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

/// Compaction's pass 3, the fold's own entry point (compaction §3): the **same** keep-newest merge
/// as [`coalesce_external_id_runs`] over the live external-id runs, but dropping the keys of every
/// entity `tombstones` names and writing the locator through a **memory-mapped** file rather than
/// an in-memory `Vec` — see the module doc for why the fold needs both and an ordinary coalesce
/// needs neither.
///
/// `entity_lo`/`entity_hi` are the caller's — the fold's own snapshot bound (`D₀`'s entity space),
/// never derived from the runs (see `merge_runs_core`'s `emit` closure for why) and never widened
/// to the live high-water past it. The carried-forward `locator_extents` cover every entity past
/// `entity_hi` (contracts §2.4); a locator sized past the snapshot swallows them and answers "no
/// external id" for an item that has one — the fatal trap this function exists not to repeat.
///
/// `tombstones` is `D₀`, the fold's tombstone clone (compaction §1) — the same set passes 1 and 2
/// execute over, never `executed` (compaction §5: `executed` is derived at publication, hours
/// after this pass has run).
pub fn fold_external_id_runs(
    runs: &[PathBuf],
    entity_lo: u64,
    entity_hi: u64,
    tombstones: &Bitmap,
    out_dir: &Path,
) -> Result<usize> {
    merge_runs_core(
        &open_runs(runs)?,
        entity_lo,
        entity_hi,
        out_dir,
        Some(tombstones),
        // The fold's span is the whole entity space, whether or not `D₀` is empty this round.
        LocatorStorage::Mapped,
    )
}

/// Open each run as a cursor, **oldest first**, holding only its mapped batches and a position.
pub(crate) fn open_runs(paths: &[PathBuf]) -> Result<Vec<RunCursor>> {
    paths.iter().map(|path| RunCursor::open(path)).collect()
}

/// A cursor over one already-sorted external-id run: `(external_id, entity)` ascending by the id
/// bytes, which is the order [`crate::flush::write_external_id_run`] writes and the sidecar
/// verifies at open.
pub(crate) struct RunCursor {
    ids: BinaryArray,
    entities: UInt32Array,
    row: usize,
}

impl RunCursor {
    /// **Mapped, not decoded**, which is compaction §3's *"every input is mmapped and uncompressed
    /// by contract"* holding for this input as it already does for the others.
    ///
    /// This read the run through `arrow::ipc::reader::FileReader` and collected every batch into
    /// the heap. For a coalesce that is bounded — the inputs are one round's runs — and for a
    /// **fold** it is the whole corpus's external ids in one anonymous allocation, which probe P1
    /// measured as the fold's peak: a resident set tracking its own output bytes, peaking inside
    /// this pass and nowhere else. The read path never had the problem; `sidecar::load_validated`
    /// has always mapped the same file and decoded it zero-copy, so this is the maintenance path
    /// adopting the reader the query path already uses rather than a new construction.
    ///
    /// A run is **one** record batch by construction ([`crate::write::RunWriter::finish`] assembles
    /// exactly one, over spools mapped as its values buffers), which is why a single-batch decoder
    /// is sufficient rather than a simplification — the sidecar relies on the same property.
    fn open(path: &Path) -> Result<Self> {
        let file = File::open(path).map_err(|source| StoreError::Io {
            path: path.to_path_buf(),
            source,
        })?;
        // SAFETY: identical justification to `ColumnsRef::load`'s mmap branch — `arc` outlives
        // every `Buffer` built from it (captured as the buffer's `Allocation`), the mapping is
        // valid for `len` bytes for its whole lifetime, and `memmap2::Mmap` never returns a null
        // base pointer.
        let mapping = unsafe { Mmap::map(&file) }.map_err(|source| StoreError::Io {
            path: path.to_path_buf(),
            source,
        })?;
        // The same reclaim bias `SegmentCursor::open` asks for, for the same reason: both callers
        // of this cursor — a coalesce's merge and the fold's pass 3 — read every run once, and the
        // sidecar that serves *requests* from these files holds its own mapping
        // (`sidecar::load_validated`), which this does not touch. See
        // `MortonSlice::advise_sequential`.
        let _ = mapping.advise(memmap2::Advice::Sequential);
        let len = mapping.len();
        let arc: Arc<Mmap> = Arc::new(mapping);
        let ptr = NonNull::new(arc.as_ptr() as *mut u8)
            .expect("memmap2::Mmap never returns a null base pointer");
        let buffer = unsafe { Buffer::from_custom_allocation(ptr, len, arc) };
        let batch = decode_single_batch(&buffer, path)?;

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
        Ok(RunCursor {
            ids,
            entities,
            row: 0,
        })
    }

    fn peek(&self) -> Option<(&[u8], u32)> {
        (self.row < self.ids.len())
            .then(|| (self.ids.value(self.row), self.entities.value(self.row)))
    }

    fn advance(&mut self) {
        self.row += 1;
    }
}

/// Merge `cursors` (oldest first) into one run and one locator under `out_dir`, streaming — the
/// ordinary maintenance shape: nothing dropped, the locator a small in-memory `Vec`. See
/// [`fold_external_id_runs`] for the fold's own call, which drops tombstoned keys and maps the
/// locator instead.
pub(crate) fn merge_runs(
    cursors: &[RunCursor],
    entity_lo: u64,
    entity_hi: u64,
    out_dir: &Path,
) -> Result<usize> {
    merge_runs_core(
        cursors,
        entity_lo,
        entity_hi,
        out_dir,
        None,
        LocatorStorage::Buffered,
    )
}

/// The one keep-newest k-way merge, shared by [`merge_runs`] and [`fold_external_id_runs`] — **no
/// second implementation of the tie-break exists**, so a fix or a regression in one path is a fix
/// or a regression in both.
///
/// **The output is exactly what a stable sort of the concatenation followed by a keep-last pass
/// produces**, which is what this replaced: ascending by key, ties broken by ascending run index
/// and then position, with only the last of each equal-key group emitted, **and then, if
/// `tombstones` names that survivor's entity, dropped entirely rather than falling back to an
/// older binding of the same key** — an older binding under decision 0047 is already a forgotten,
/// deleted holder, tombstoned or not, so there is nothing to fall back *to*. That equivalence is
/// the whole correctness argument for the merge half — see
/// `coalesce_runs::a_key_in_two_runs_keeps_the_newest_binding`, where the tie-break is pinned, and
/// `merge_execution::the_external_id_runs_coalesce_in_key_order` for the ordering —
/// `fold_external_ids::a_tombstoned_newest_binding_drops_the_key_rather_than_falling_back` is the
/// tombstone half's own pin.
///
/// **A heap that clones the key, rather than a linear scan or a borrow-free tournament.** A scan
/// over *k* per row is O(n·k), which is fine at a merge's `tier_width` of 4 and not at a fold's
/// live run count; a tournament over borrowed keys cannot be expressed without either unsafe or a
/// hand-rolled sift, since the comparison borrows the cursors the pop mutates. External ids are
/// capped at 64 bytes (contracts §1), and every one of the fold's inputs is read through a mapping
/// rather than a rate-limited stream — there is no read to throttle between, and the mitigation
/// for the page-cache pressure that leaves is `madvise(MADV_SEQUENTIAL)` (decision 0052), not a
/// rate (compaction §6.1's 128 MiB/s figure was evidence about pollution, not a mechanism this
/// design can set — refuted at r5). So one small allocation per key buys O(n log k) and an
/// obviously correct pop, and memory stays O(k) regardless of which caller this is.
///
/// **The filter and the locator's storage are two parameters, not one.** They happen to move
/// together at this crate's only two call sites — a coalesce filters nothing and its span is
/// policy-bounded, a fold filters `D₀` and its span is the whole entity space — but they are
/// independent properties, and inferring the second from the first would put a 4 GB decision behind
/// a predicate about deletions. A fold with an *empty* `D₀` is an ordinary case (compaction §5: an
/// entity whose row survives is simply not retired this round), and it still needs the mapped
/// locator.
fn merge_runs_core(
    cursors: &[RunCursor],
    entity_lo: u64,
    entity_hi: u64,
    out_dir: &Path,
    tombstones: Option<&Bitmap>,
    storage: LocatorStorage,
) -> Result<usize> {
    let span =
        usize::try_from(entity_hi - entity_lo + 1).map_err(|_| StoreError::MalformedBundle {
            detail: format!("coalesce: entity span {entity_lo}..={entity_hi} is too wide"),
        })?;

    // Cloned so the caller's cursors keep their positions. Both arrays are views over the run's
    // mapping, so a clone is two refcount bumps and no bytes.
    let mut cursors: Vec<RunCursor> = cursors
        .iter()
        .map(|c| RunCursor {
            ids: c.ids.clone(),
            entities: c.entities.clone(),
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

    let locator_path = out_dir.join("ext-locator.u32");
    let io = |source| StoreError::Io {
        path: out_dir.join("external-ids.arrow"),
        source,
    };
    let mut writer = RunWriter::create(&out_dir.join("external-ids.arrow")).map_err(io)?;
    let mut locator = match storage {
        LocatorStorage::Mapped => LocatorSink::mapped(&locator_path, span as u64)?,
        LocatorStorage::Buffered => LocatorSink::buffered(span),
    };
    let mut pending: Option<(Vec<u8>, u32)> = None;
    let mut rows = 0usize;

    let emit = |writer: &mut RunWriter,
                locator: &mut LocatorSink,
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
        locator.set(&locator_path, slot, *rows as u32)?;
        writer
            .append(key, entity)
            .map_err(|source| StoreError::Io {
                path: out_dir.join("external-ids.arrow"),
                source,
            })?;
        *rows += 1;
        Ok(())
    };

    // An entity the caller's tombstone set names is dropped — not written to run 0, not given a
    // locator slot (whose sentinel fill then answers "no external id" for it, correctly). See the
    // module doc: this is the artefact half of what the fold needs here, never Rule F's retirement
    // itself.
    let is_tombstoned = |entity: u32| tombstones.is_some_and(|t| t.contains(entity));

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
            if prev_key != key && !is_tombstoned(prev_entity) {
                emit(&mut writer, &mut locator, &mut rows, &prev_key, prev_entity)?;
            }
        }
        pending = Some((key, entity));
    }
    if let Some((key, entity)) = pending {
        if !is_tombstoned(entity) {
            emit(&mut writer, &mut locator, &mut rows, &key, entity)?;
        }
    }

    let written = writer.finish().map_err(io)?;
    if written != rows {
        return Err(StoreError::MalformedBundle {
            detail: format!(
                "coalesce: the run writer wrote {written} rows where the merge emitted {rows}"
            ),
        });
    }
    locator.check(rows)?;
    locator.finish(&locator_path)?;
    Ok(rows)
}

/// The slots holding an ordinal, and how many of those are not below `rows`.
fn tally(slots: impl Iterator<Item = u32>, rows: usize) -> (usize, usize) {
    slots
        .filter(|&slot| slot != ROW_ABSENT)
        .fold((0, 0), |(set, beyond), slot| {
            (set + 1, beyond + usize::from(slot as usize >= rows))
        })
}

/// Where a merge's (or the fold's) surviving `entity → ordinal` pairs go while the merge runs.
///
/// An ordinary coalesce's span is bounded by the maintenance policy that caps external-id run
/// count (contracts §2.4), so an in-memory array costs nothing worth avoiding — [`Self::Buffered`]
/// is exactly what [`write_u32_array`] wrote directly before this type existed, same bytes, same
/// call, now behind one match arm. The fold's span is the whole entity space, so it uses
/// [`Self::Mapped`] instead — see [`crate::locator`]'s module doc for why that is a distinct
/// writer rather than a generalisation of `PermutationWriter`.
enum LocatorSink {
    Buffered(Vec<u32>),
    Mapped(LocatorWriter),
}

/// Which of the two [`LocatorSink`]s a caller wants — chosen by the **span**, never inferred from
/// whether that caller also filters.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum LocatorStorage {
    /// A span bounded by the maintenance policy that caps run count: an in-memory `Vec` costs
    /// nothing worth avoiding.
    Buffered,
    /// A corpus-sized span — 4 GB at 10⁹ — where a `Vec` is anonymous memory the kernel can only
    /// page to swap, and a mapping is reclaimable page cache.
    Mapped,
}

impl LocatorSink {
    fn buffered(span: usize) -> Self {
        LocatorSink::Buffered(vec![ROW_ABSENT; span])
    }

    fn mapped(path: &Path, span: u64) -> Result<Self> {
        LocatorWriter::create(path, span)
            .map(LocatorSink::Mapped)
            .map_err(|source| StoreError::Io {
                path: path.to_path_buf(),
                source,
            })
    }

    fn set(&mut self, path: &Path, slot: usize, ordinal: u32) -> Result<()> {
        match self {
            LocatorSink::Buffered(v) => {
                v[slot] = ordinal;
                Ok(())
            }
            LocatorSink::Mapped(w) => {
                w.set(slot as u64, ordinal)
                    .map_err(|source| StoreError::Io {
                        path: path.to_path_buf(),
                        source,
                    })
            }
        }
    }

    /// Exactly `rows` slots hold an ordinal, each below `rows`. An entity bound to two keys sets
    /// its slot twice and leaves fewer, and the reverse direction would then name one of its keys
    /// only.
    fn check(&self, rows: usize) -> Result<()> {
        let (set, beyond) = match self {
            LocatorSink::Buffered(v) => tally(v.iter().copied(), rows),
            LocatorSink::Mapped(LocatorWriter::Empty) => (0, 0),
            LocatorSink::Mapped(LocatorWriter::Mapped { map, .. }) => tally(
                map.chunks_exact(4).map(|b| u32::from_le_bytes([b[0], b[1], b[2], b[3]])),
                rows,
            ),
        };
        if set != rows || beyond != 0 {
            return Err(StoreError::MalformedBundle {
                detail: format!(
                    "coalesce: the locator sets {set} slots, {beyond} of them past the run's end, \
                     for a run of {rows} rows"
                ),
            });
        }
        Ok(())
    }

    fn finish(self, path: &Path) -> Result<()> {
        match self {
            LocatorSink::Buffered(v) => write_u32_array(path, &v),
            LocatorSink::Mapped(w) => w.finish().map_err(|source| StoreError::Io {
                path: path.to_path_buf(),
                source,
            }),
        }
    }
}
