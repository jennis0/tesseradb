//! Coalescing external-id runs: the half of a merge that is about **entity** space, reusable on
//! its own.
//!
//! A run is a file sorted by *caller-supplied* keys (contracts §2.4), and a lookup searches every
//! run whose own bounds admit the key — so the run count is a term in the ingest duplicate check
//! and in the drill-down, both of which a 90 s flush tick grows by ~960 a day. Bounding it is what
//! this module exists for, and it is deliberately separable from row space: the row-space merge
//! ([`crate::merge::execute_merge`]) calls the same two functions for its own runs, and the
//! entity-space coalesce publication calls them without touching a segment at all.
//!
//! **Nothing here retires anything.** The keep-newest rule below drops an *older binding of the
//! same key*, which decision 0047 makes a forgotten, deleted holder — not a live entity's row, not
//! a posting, not an overlay entry. A function here that dropped a key because its entity was
//! deleted would be performing the compaction fold.

use std::fs::File;
use std::path::{Path, PathBuf};

use arrow::array::{Array, BinaryArray, UInt32Array};
use arrow::ipc::reader::FileReader;

use tessera_types::ROW_ABSENT;

use crate::error::{Result, StoreError};
use crate::flush::{write_external_id_run, write_u32_array};

/// Coalesce `runs` — prefix-relative order, **oldest first** — into one run and one locator
/// under `out_dir`, covering `[entity_lo, entity_hi]`. Returns the coalesced run's row count.
///
/// The entity-space publication's whole file-writing half: see [`write_coalesced_run`] for the
/// keep-newest rule and for why the span is the caller's rather than derived.
pub fn coalesce_external_id_runs(
    runs: &[PathBuf],
    entity_lo: u64,
    entity_hi: u64,
    out_dir: &Path,
) -> Result<usize> {
    write_coalesced_run(read_runs(runs)?, entity_lo, entity_hi, out_dir)
}

/// Read every input run's `(external_id, entity)` pairs, **oldest run first**.
///
/// The order is the keep-newest rule's only input: [`write_coalesced_run`] resolves a key present
/// in two runs by taking the last one it sees, so a caller that reversed this would resurrect a
/// forgotten holder and answer the wrong entity for a re-ingested external id.
pub(crate) fn read_runs(paths: &[PathBuf]) -> Result<Vec<(Vec<u8>, u32)>> {
    let mut forward: Vec<(Vec<u8>, u32)> = Vec::new();
    for path in paths {
        forward.extend(read_external_id_run(path)?);
    }
    Ok(forward)
}

/// Write `forward` as one run at `out_dir/external-ids.arrow`, with its reverse locator over
/// `[entity_lo, entity_hi]` at `out_dir/ext-locator.u32`. Returns the run's row count.
///
/// **The runs merge by caller key, because that is the only order a run has** (contracts §2.4).
/// Unlike an extent, a run cannot be ordered against its neighbours — nothing coordinates what
/// keys a caller supplies — so coalescing is a sort over the bytes, and the reader binary-searches
/// the result.
///
/// **A key present in two inputs keeps the newest binding** (decision 0047). Delete plus
/// re-ingest re-binds an external id, so the older holder is a forgotten, deleted entity: the
/// reader resolves newest-run-first, and a coalesced run must answer exactly as the runs it
/// replaced did. `forward` arrives oldest-run-first, so a **stable** sort keeps that order within
/// equal keys and the keep-last pass below selects the newest. An unstable sort here is a silent
/// coin-flip between a live entity and a deleted one.
///
/// **The locator's span is the caller's, never derived from the entities present.** An entity
/// ingested without an external id (contracts §3.4 r6's ordinary case) has no pair here at all, so
/// a span taken from `forward` would end short of it — and an entity past every locator extent but
/// below the live high-water is an *inconsistency* to the drill-down, not an absent external id.
/// The span is the union of the consumed extents' spans, which is exactly the entity range they
/// were answering for.
pub(crate) fn write_coalesced_run(
    mut forward: Vec<(Vec<u8>, u32)>,
    entity_lo: u64,
    entity_hi: u64,
    out_dir: &Path,
) -> Result<usize> {
    let span =
        usize::try_from(entity_hi - entity_lo + 1).map_err(|_| StoreError::MalformedBundle {
            detail: format!("coalesce: entity span {entity_lo}..={entity_hi} is too wide"),
        })?;

    forward.sort_by(|a, b| a.0.cmp(&b.0));
    let mut write = 0usize;
    for read in 0..forward.len() {
        if read + 1 < forward.len() && forward[read + 1].0 == forward[read].0 {
            continue; // a newer binding for the same key follows; drop this one
        }
        forward.swap(write, read);
        write += 1;
    }
    forward.truncate(write);

    let rows: Vec<(&[u8], u32)> = forward.iter().map(|(id, e)| (id.as_slice(), *e)).collect();
    write_external_id_run(&out_dir.join("external-ids.arrow"), &rows)?;

    let mut locator = vec![ROW_ABSENT; span];
    for (ordinal, (_, entity)) in forward.iter().enumerate() {
        let slot = (*entity as u64).checked_sub(entity_lo).and_then(|i| {
            let i = usize::try_from(i).ok()?;
            (i < span).then_some(i)
        });
        let slot = slot.ok_or_else(|| StoreError::MalformedBundle {
            detail: format!(
                "coalesce: entity {entity} holds an external id but falls outside the locator \
                 span {entity_lo}..={entity_hi} — the reverse direction would have no home for it"
            ),
        })?;
        locator[slot] = ordinal as u32;
    }
    write_u32_array(&out_dir.join("ext-locator.u32"), &locator)?;

    Ok(forward.len())
}

/// Read one external-id run back as `(external_id, entity)` pairs.
fn read_external_id_run(path: &Path) -> Result<Vec<(Vec<u8>, u32)>> {
    let file = File::open(path).map_err(|source| StoreError::Io {
        path: path.to_path_buf(),
        source,
    })?;
    let reader = FileReader::try_new(file, None).map_err(|e| StoreError::MalformedBundle {
        detail: format!("external-ids.arrow at {}: {e}", path.display()),
    })?;
    let mut out = Vec::new();
    for batch in reader {
        let batch = batch.map_err(|e| StoreError::MalformedBundle {
            detail: format!("external-ids.arrow at {}: {e}", path.display()),
        })?;
        let ids = batch
            .column(0)
            .as_any()
            .downcast_ref::<BinaryArray>()
            .ok_or_else(|| StoreError::MalformedBundle {
                detail: "external-ids.arrow: column 0 is not binary".to_string(),
            })?;
        let entities = batch
            .column(1)
            .as_any()
            .downcast_ref::<UInt32Array>()
            .ok_or_else(|| StoreError::MalformedBundle {
                detail: "external-ids.arrow: column 1 is not uint32".to_string(),
            })?;
        for i in 0..batch.num_rows() {
            out.push((ids.value(i).to_vec(), entities.value(i)));
        }
    }
    Ok(out)
}
