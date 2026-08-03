//! The caller's external-ID namespace, as a **sidecar** (contracts §0.3 deviation 9, §2.4 r6).
//!
//! **TRANSITIONAL — a placeholder for a future adopted per-point metadata store** (owner ruling,
//! 2026-07-29). This is deliberately the simplest structure that satisfies its two callers, and
//! it occupies the same slot as design §8.3's vector sidecar and §10.3's per-interaction routing
//! row; the eventual store serves all of them. **Extend the replacement, not this.** Everything
//! the rest of the system knows about external IDs is [`ExternalIdSidecar::resolve`] and
//! [`ExternalIdSidecar::external_id_of`] — keep it that way, so the storage behind them can be
//! swapped by changing this file and its constructor call. Design Appendix D does not forbid that
//! adoption: it rejects adopting external systems for the *access-control layer*, and this store
//! is read only after the visibility test has already returned "visible", so it never
//! participates in masking.
//!
//! **One identity, supplied or derived.** This is a *translation table* between two
//! representations of one identity, not a store of identities: an item whose caller supplied no
//! key has its `tessera_id` as its identifier, occupies no run row, and carries a
//! `0xFFFFFFFF` locator slot — the ordinary case, not a missing value. Never manufacture an
//! external ID for an item that has none.
//!
//! Two directions, neither on the viewport path: `external_id → entity` for
//! `/control/changes` past WAL retention and `/control/ingest`'s duplicate check, and
//! `entity → external_id` (through `ext-locator.u32`) for the `/v1/items` drill-down. There is
//! **no `tessera_id → entity` direction** — inversion is a pure function of the deployment key
//! and touches no file at all.
//!
//! Nothing is mapped, scanned or verified until the first resolution, and then only the one
//! run the key falls in. At 10⁹ the previous eager mmap-and-linear-scan put 18.9 GB into the
//! resident set at `Engine::open` for a structure the per-viewport path never touches; a single
//! lock over the whole family would have restored most of that on the first click — laziness is
//! per **run**, tracked by each run's own [`OnceLock`], not one lock over the family.
//!
//! Integrity does *not* relax. A corrupted mapping suppresses the wrong item, so a run's
//! digest **and its sortedness** are both verified before any answer comes out of it — a digest
//! proves the file is the one MANIFEST named, sortedness proves the binary search returns the
//! right answer, and a build bug emitting an out-of-order run produces a correctly-digested
//! file. Every failure is a typed error ([`StoreError::InvalidSidecar`]), never a `None` that
//! would read as "unknown external ID".
//!
//! **No error detail here names an entity ID.** These strings are the ones that would reach a
//! server log — and contracts §4 has the byte-scanner sweep payloads *and logs* for entity IDs
//! (I10). Every message therefore states the structural facts (which file, how many slots, the
//! high-water it was compared against) and not the identifier: a corrupt sidecar is a systematic
//! build or flush fault, so the file and the shape of the inconsistency are what an operator
//! needs, and naming the item buys nothing an entity-independent message does not. The server
//! separately refuses to forward any of this to a client — see `tessera_server::error`.

use std::fs::File;
use std::path::{Path, PathBuf};
use std::ptr::NonNull;
use std::sync::{Arc, OnceLock};

use arrow::array::{Array, BinaryArray, UInt32Array};
use arrow::buffer::Buffer;
use arrow::datatypes::DataType;
use arrow::record_batch::RecordBatch;
use memmap2::Mmap;
use sha2::{Digest, Sha256};

use tessera_types::EntityId;

use crate::error::{Result, StoreError};
use crate::manifest::{Manifest, SegmentsManifest};
use crate::read::decode_single_batch;

/// `0xFFFFFFFF`: the locator sentinel for "this entity has no caller-supplied external ID"
/// (contracts §2.4 r6) — the **ordinary** case for an item whose identity is its `tessera_id`,
/// never a missing-data marker.
const LOCATOR_NONE: u32 = 0xFFFF_FFFF;

/// One peeked run's bounds: `(first_key, last_key, row_count)`.
type ExtentBounds = (Vec<u8>, Vec<u8>, usize);

/// One `external-ids-<n>.arrow` run's identity: its path and the digest a well-formed bundle
/// records for it in a manifest's `files` map. **Internal to this module** — see the module doc
/// for why this must never leak into `tessera-engine` or `tessera-server`.
#[derive(Debug, Clone)]
struct RunDesc {
    path: PathBuf,
    /// Lowercase hex SHA-256, matching [`crate::manifest::FileDigest::sha256`].
    digest: String,
}

impl RunDesc {
    fn new(path: impl Into<PathBuf>, digest: impl Into<String>) -> Self {
        RunDesc {
            path: path.into(),
            digest: digest.into(),
        }
    }
}

/// The `entities/ext-locator.u32` file's identity plus its declared length (contracts §2.4 r6:
/// "length `entity_id_high_water` at build"). Internal, same reasoning as [`RunDesc`].
#[derive(Debug, Clone)]
struct LocatorDesc {
    path: PathBuf,
    digest: String,
    /// Entries — the file's length in `u32`s, i.e. `entity_id_high_water` at build.
    len: u64,
}

/// A digest-and-sortedness-verified run, held zero-copy over its mmap (identical technique to
/// `crate::read::ColumnsRef` — see that type's doc).
struct ValidatedExtent {
    batch: RecordBatch,
}

impl ValidatedExtent {
    fn ext_col(&self) -> &BinaryArray {
        self.batch
            .column(0)
            .as_any()
            .downcast_ref::<BinaryArray>()
            .expect("validated at load")
    }

    fn ent_col(&self) -> &UInt32Array {
        self.batch
            .column(1)
            .as_any()
            .downcast_ref::<UInt32Array>()
            .expect("validated at load")
    }

    fn len(&self) -> usize {
        self.batch.num_rows()
    }

    fn key(&self, idx: usize) -> &[u8] {
        self.ext_col().value(idx)
    }

    fn entity(&self, idx: usize) -> u32 {
        self.ent_col().value(idx)
    }

    fn resolve(&self, external_id: &[u8]) -> Option<EntityId> {
        let ext_col = self.ext_col();
        let idx = binary_search_by(self.len(), |i| ext_col.value(i).cmp(external_id)).ok()?;
        Some(EntityId::new(self.entity(idx) as u64))
    }
}

/// `[0, n)` binary search parameterised on a comparator (see the former `external_ids.rs` for
/// the identical rationale: `BinaryArray` has no materialised `&[&[u8]]` to slice over).
fn binary_search_by(
    n: usize,
    mut cmp: impl FnMut(usize) -> std::cmp::Ordering,
) -> std::result::Result<usize, usize> {
    let mut lo = 0usize;
    let mut hi = n;
    while lo < hi {
        let mid = lo + (hi - lo) / 2;
        match cmp(mid) {
            std::cmp::Ordering::Less => lo = mid + 1,
            std::cmp::Ordering::Greater => hi = mid,
            std::cmp::Ordering::Equal => return Ok(mid),
        }
    }
    Err(lo)
}

/// One run's lazily-opened, per-run state (Critical C-6). `cell` is populated on first
/// use and never again — a corrupt run stays corrupt (typed error, cached as a message rather
/// than a live `StoreError` so this struct needs no `Clone` impl on `StoreError`'s I/O variant),
/// a valid one stays valid (the bundle contract makes every named file immutable once
/// published).
struct RunSlot {
    desc: RunDesc,
    cell: OnceLock<std::result::Result<ValidatedExtent, String>>,
}

impl RunSlot {
    fn new(desc: RunDesc) -> Self {
        RunSlot {
            desc,
            cell: OnceLock::new(),
        }
    }

    fn is_open(&self) -> bool {
        self.cell.get().is_some()
    }

    /// Open (if not already), verifying digest **and** sortedness in one pass over the mapped
    /// bytes (Critical C-1 — the digest proves the file is the one MANIFEST named; sortedness
    /// proves the binary search over it returns the right answer, and neither subsumes the
    /// other). Never returns a bare `None` for a failure — always a typed
    /// [`StoreError::InvalidSidecar`].
    fn get_or_load(&self) -> Result<&ValidatedExtent> {
        let result = self.cell.get_or_init(|| load_validated(&self.desc));
        result
            .as_ref()
            .map_err(|detail| StoreError::InvalidSidecar {
                path: self.desc.path.clone(),
                detail: detail.clone(),
            })
    }
}

fn load_validated(desc: &RunDesc) -> std::result::Result<ValidatedExtent, String> {
    let file = File::open(&desc.path).map_err(|e| format!("io error: {e}"))?;
    // SAFETY: identical justification to `ColumnsRef::load`'s mmap branch — `arc` outlives every
    // `Buffer` built from it, the mapping is valid for `len` bytes for its whole lifetime, and
    // `memmap2::Mmap` never returns a null base pointer.
    let mapping = unsafe { Mmap::map(&file) }.map_err(|e| format!("io error: {e}"))?;

    // Digest first, over the raw mapped bytes, in one linear pass; sortedness (below) then reads
    // the same bytes through the decoded arrays — the OS has already paged them in, so the check
    // costs nothing the digest pass didn't already cost (Critical C-1's "one sequential pass").
    let actual_digest = hex_digest(Sha256::digest(&mapping[..]).as_slice());
    if actual_digest != desc.digest {
        return Err(format!(
            "digest mismatch: manifest says {}, computed {actual_digest}",
            desc.digest
        ));
    }

    let len = mapping.len();
    let arc: Arc<Mmap> = Arc::new(mapping);
    let ptr = NonNull::new(arc.as_ptr() as *mut u8)
        .expect("memmap2::Mmap never returns a null base pointer");
    let buffer = unsafe { Buffer::from_custom_allocation(ptr, len, arc) };

    let batch = decode_single_batch(&buffer, &desc.path).map_err(|e| e.to_string())?;
    validate_schema(&batch).map_err(|e| e.to_string())?;
    validate_sorted(&batch).map_err(|e| e.to_string())?;

    Ok(ValidatedExtent { batch })
}

fn validate_schema(batch: &RecordBatch) -> std::result::Result<(), String> {
    let schema = batch.schema_ref();
    if schema.fields().len() != 2 {
        return Err(format!(
            "expected exactly 2 columns (external_id, entity_id), found {}",
            schema.fields().len()
        ));
    }
    let ext_field = schema.field(0);
    if ext_field.data_type() != &DataType::Binary || ext_field.is_nullable() {
        return Err(format!(
            "column 0 must be non-nullable Binary, found {:?} (nullable: {})",
            ext_field.data_type(),
            ext_field.is_nullable()
        ));
    }
    let ent_field = schema.field(1);
    if ent_field.data_type() != &DataType::UInt32 || ent_field.is_nullable() {
        return Err(format!(
            "column 1 must be non-nullable UInt32, found {:?} (nullable: {})",
            ent_field.data_type(),
            ent_field.is_nullable()
        ));
    }
    if batch.column(0).null_count() != 0 || batch.column(1).null_count() != 0 {
        return Err("columns must have no nulls".to_string());
    }
    Ok(())
}

/// R4 guarantees each run is individually sorted ascending by external-id bytes. This is
/// authorisation-bearing (a mis-resolved external id in `/control/changes` denies the wrong
/// entity and leaves the intended target visible), so it fails closed rather than trust the
/// digest to have implied it — **it does not** (Critical C-1).
fn validate_sorted(batch: &RecordBatch) -> std::result::Result<(), String> {
    let ext_col = batch
        .column(0)
        .as_any()
        .downcast_ref::<BinaryArray>()
        .expect("schema validated");
    for i in 1..ext_col.len() {
        if ext_col.value(i - 1) >= ext_col.value(i) {
            return Err(format!(
                "external_id column is not strictly ascending at row {i} (row {} >= row {i})",
                i - 1
            ));
        }
    }
    Ok(())
}

fn hex_digest(digest: &[u8]) -> String {
    use std::fmt::Write;
    let mut out = String::with_capacity(digest.len() * 2);
    for byte in digest {
        let _ = write!(out, "{byte:02x}");
    }
    out
}

/// A cheap, unvalidated peek at one run's bounds: first key, last key, row count. Reads the
/// Arrow IPC footer and the two boundary values only — **no digest, no sortedness scan, no
/// caching** (Ruling B: nothing clever). Used purely to select which single run a lookup
/// falls in and to validate the run *list's* ordering (Critical C-6/point 4); the run
/// actually resolved into is always separately, fully validated by [`RunSlot::get_or_load`]
/// before an answer is returned. `Ok(None)` means the run has zero rows.
fn peek_bounds(path: &Path) -> Result<Option<ExtentBounds>> {
    let peek = || -> std::result::Result<Option<ExtentBounds>, String> {
        let file = File::open(path).map_err(|e| format!("io error: {e}"))?;
        let mapping = unsafe { Mmap::map(&file) }.map_err(|e| format!("io error: {e}"))?;
        let len = mapping.len();
        let arc: Arc<Mmap> = Arc::new(mapping);
        let ptr = NonNull::new(arc.as_ptr() as *mut u8)
            .expect("memmap2::Mmap never returns a null base pointer");
        let buffer = unsafe { Buffer::from_custom_allocation(ptr, len, arc) };
        let batch = decode_single_batch(&buffer, path).map_err(|e| e.to_string())?;
        validate_schema(&batch)?;
        let ext_col = batch
            .column(0)
            .as_any()
            .downcast_ref::<BinaryArray>()
            .expect("schema validated");
        let n = ext_col.len();
        if n == 0 {
            return Ok(None);
        }
        Ok(Some((
            ext_col.value(0).to_vec(),
            ext_col.value(n - 1).to_vec(),
            n,
        )))
    };
    peek().map_err(|detail| StoreError::InvalidSidecar {
        path: path.to_path_buf(),
        detail,
    })
}

/// One run's peeked bounds (or `None` if empty), paired with its resident, digest-verified
/// full open on demand.
struct RunKeyScan {
    /// Per-run: `Some((first_key, last_key, row_count))`, or `None` if that run is empty.
    bounds: Vec<Option<ExtentBounds>>,
}

/// Peek every run's bounds, in order, validating that non-empty runs partition one
/// ascending order across the whole family (Critical: cross-run ordering must be validated,
/// not assumed — point 4). Fresh on every call: no cache (Ruling B).
fn scan_run_keys(descs: &[RunDesc]) -> Result<RunKeyScan> {
    let mut bounds = Vec::with_capacity(descs.len());
    let mut prev_non_empty: Option<(usize, Vec<u8>)> = None;
    for (idx, desc) in descs.iter().enumerate() {
        let this = peek_bounds(&desc.path)?;
        if let Some((first, last, _)) = &this {
            if let Some((prev_idx, prev_last)) = &prev_non_empty {
                if prev_last.as_slice() >= first.as_slice() {
                    return Err(StoreError::InvalidSidecar {
                        path: desc.path.clone(),
                        detail: format!(
                            "run {prev_idx}'s last key is not strictly less than run \
                             {idx}'s first key — runs must partition one ascending order"
                        ),
                    });
                }
            }
            prev_non_empty = Some((idx, last.clone()));
        }
        bounds.push(this);
    }
    Ok(RunKeyScan { bounds })
}

/// The lazily-opened `entities/ext-locator.u32` sidecar: one raw `u32` array, no header, length
/// `len` (`entity_id_high_water` at build), `locator[entity_id]` gives that entity's ordinal in
/// the concatenated sorted external-id runs, or [`LOCATOR_NONE`] if the entity has no
/// caller-supplied external ID (contracts §2.4 r6).
struct LocatorSlot {
    desc: LocatorDesc,
    cell: OnceLock<std::result::Result<Mmap, String>>,
}

impl LocatorSlot {
    fn new(desc: LocatorDesc) -> Self {
        LocatorSlot {
            desc,
            cell: OnceLock::new(),
        }
    }

    fn is_open(&self) -> bool {
        self.cell.get().is_some()
    }

    fn get_or_load(&self) -> Result<&[u32]> {
        let result = self.cell.get_or_init(|| load_locator(&self.desc));
        let mapping = result
            .as_ref()
            .map_err(|detail| StoreError::InvalidSidecar {
                path: self.desc.path.clone(),
                detail: detail.clone(),
            })?;
        // SAFETY: length is a checked multiple of 4 and matches `desc.len * 4` exactly
        // (validated at `load_locator`); the mmap base is page-aligned.
        Ok(unsafe {
            std::slice::from_raw_parts(mapping.as_ptr() as *const u32, self.desc.len as usize)
        })
    }

    /// This entity's ordinal into the concatenated external-id runs, or `None` if it has no
    /// external ID ([`LOCATOR_NONE`] — the ordinary case, Ruling A). A corrupt or out-of-range
    /// locator is a typed error, never a `None` masquerading as "no external ID".
    fn ordinal_of(&self, entity: EntityId) -> Result<Option<u32>> {
        let idx = entity.raw();
        if idx >= self.desc.len {
            // Not covered by this bundle's locator at all — either an entity ingested after the
            // build (the caller's job, not this sidecar's, to have checked its live map first)
            // or genuinely beyond the allocator high-water. Not this module's error to raise.
            return Ok(None);
        }
        let slice = self.get_or_load()?;
        let ord = slice[idx as usize];
        if ord == LOCATOR_NONE {
            Ok(None)
        } else {
            Ok(Some(ord))
        }
    }
}

fn load_locator(desc: &LocatorDesc) -> std::result::Result<Mmap, String> {
    let file = File::open(&desc.path).map_err(|e| format!("io error: {e}"))?;
    let mapping = unsafe { Mmap::map(&file) }.map_err(|e| format!("io error: {e}"))?;
    let expected_len = (desc.len as usize)
        .checked_mul(4)
        .ok_or_else(|| "locator length overflows".to_string())?;
    if mapping.len() != expected_len {
        return Err(format!(
            "locator length {} bytes does not match manifest-declared {expected_len} \
             ({} entries)",
            mapping.len(),
            desc.len
        ));
    }
    let actual_digest = hex_digest(Sha256::digest(&mapping[..]).as_slice());
    if actual_digest != desc.digest {
        return Err(format!(
            "digest mismatch: manifest says {}, computed {actual_digest}",
            desc.digest
        ));
    }
    Ok(mapping)
}

/// The external-ID sidecar for one partition: `external_id → entity` for the control plane
/// (`/control/changes` past WAL retention, `/control/ingest`'s duplicate check) and
/// `entity → external_id` (via the locator) for `/v1/items` drill-down. See the module doc for
/// the transitional-placeholder framing and the design invariants this must never regress.
pub struct ExternalIdSidecar {
    runs: Vec<RunSlot>,
    locator: Option<LocatorSlot>,
}

impl ExternalIdSidecar {
    /// Build a sidecar over `runs`, none of which are opened, mapped or verified by this call
    /// (Critical C-6's laziness starts here). No locator — `external_id_of` always returns
    /// `Ok(None)`. Exposed for this module's own tests; the type held by [`RunDesc`] is
    /// private to this crate, so this constructor cannot be called, and no run descriptor can
    /// be named, from `tessera-engine` or `tessera-server`.
    fn deferred(runs: Vec<RunDesc>) -> Self {
        ExternalIdSidecar {
            runs: runs.into_iter().map(RunSlot::new).collect(),
            locator: None,
        }
    }

    fn with_locator(runs: Vec<RunDesc>, locator: LocatorDesc) -> Self {
        ExternalIdSidecar {
            runs: runs.into_iter().map(RunSlot::new).collect(),
            locator: Some(LocatorSlot::new(locator)),
        }
    }

    /// The public constructor: build the sidecar directly from the bundle's top-level manifest,
    /// the partition's side-manifest, and the bundle's prefix directory. **This is the only
    /// thing `Engine::open` calls** — no `RunDesc`, digest, ordinal or file path is handed
    /// back to the caller; replacing the storage behind this sidecar is a change to this file
    /// plus this constructor's call site, nothing else (Ruling B's acceptance test).
    ///
    /// A deployment whose callers supplied no external IDs writes no runs and no locator at
    /// all (contracts §2.4 r6) — `external_id_runs` empty is not an error, it degenerates to
    /// a sidecar that always answers `Ok(None)` in both directions.
    pub fn deferred_from_manifest(
        bundle_manifest: &Manifest,
        partition_manifest: &SegmentsManifest,
        prefix_dir: &Path,
    ) -> Result<Self> {
        let mut runs = Vec::with_capacity(partition_manifest.external_id_runs.len());
        for rel in &partition_manifest.external_id_runs {
            let digest = partition_manifest
                .files
                .get(rel)
                .or_else(|| bundle_manifest.files.get(rel))
                .ok_or_else(|| StoreError::UnverifiedFile {
                    path: prefix_dir.join(rel),
                })?;
            runs.push(RunDesc::new(prefix_dir.join(rel), digest.sha256.clone()));
        }

        if runs.is_empty() {
            return Ok(Self::deferred(runs));
        }

        // The locator has no dedicated manifest field (contracts §2.4 r6 gives it a fixed name,
        // no `<k>` suffix); it always lives alongside the runs in the same `entities/`
        // directory, so its prefix-relative path is derived from theirs rather than assumed from
        // a partition hash this constructor deliberately does not need to know.
        let first_rel = &partition_manifest.external_id_runs[0];
        let locator_rel = match first_rel.rsplit_once('/') {
            Some((dir, _)) => format!("{dir}/ext-locator.u32"),
            None => "ext-locator.u32".to_string(),
        };
        let locator_digest = partition_manifest
            .files
            .get(&locator_rel)
            .or_else(|| bundle_manifest.files.get(&locator_rel))
            .ok_or_else(|| StoreError::UnverifiedFile {
                path: prefix_dir.join(&locator_rel),
            })?;
        let locator = LocatorDesc {
            path: prefix_dir.join(&locator_rel),
            digest: locator_digest.sha256.clone(),
            len: bundle_manifest.entity_id_high_water,
        };

        Ok(Self::with_locator(runs, locator))
    }

    /// `true` if at least one run (or the locator) has been opened.
    pub fn is_open(&self) -> bool {
        self.open_extents() > 0 || self.locator.as_ref().is_some_and(|l| l.is_open())
    }

    /// How many runs have been fully opened (mapped, digest- and sortedness-verified) so far
    /// — the observable behind the residency claim, which is that steady state is *base + one
    /// run* rather than the whole family.
    pub fn open_extents(&self) -> usize {
        self.runs.iter().filter(|e| e.is_open()).count()
    }

    /// Resolve `external_id` to its entity id, or `Ok(None)` if it names nothing in this
    /// sidecar. Every failure — corrupt run, out-of-order run, mismatched digest, a
    /// shuffled run list — is `Err(StoreError::InvalidSidecar)`, never folded into `Ok(None)`
    /// (a fail-closed control-plane caller must not read "corrupt" as "not found").
    pub fn resolve(&self, external_id: &[u8]) -> Result<Option<EntityId>> {
        if self.runs.is_empty() {
            return Ok(None);
        }
        let plain: Vec<RunDesc> = self.runs.iter().map(|e| e.desc.clone()).collect();
        let scan = scan_run_keys(&plain)?;

        let Some(idx) = scan.bounds.iter().position(|b| {
            b.as_ref()
                .is_some_and(|(_, last, _)| last.as_slice() >= external_id)
        }) else {
            return Ok(None);
        };

        let run = self.runs[idx].get_or_load()?;
        Ok(run.resolve(external_id))
    }

    /// Resolve many external ids in one batched pass over the bundle — `/control/ingest`'s
    /// duplicate check (contracts §3.1 r6), which must not open one run per row against a
    /// batch that can run to thousands of keys. `scan_run_keys` runs exactly once regardless of
    /// batch size, and each run is opened (verified, mapped) at most once even if many keys
    /// fall inside it: the input is sorted internally so the resolved keys visit the runs in
    /// one ascending walk, mirroring how the runs themselves partition ascending order.
    ///
    /// Returns one `Option<EntityId>` per input key, in the caller's original order — the input
    /// need not be pre-sorted. Every failure is `Err(StoreError::InvalidSidecar)`, exactly as
    /// [`Self::resolve`]: a batch of otherwise-fine keys must not read as "all absent" because one
    /// run is corrupt.
    pub fn resolve_many(&self, external_ids: &[Vec<u8>]) -> Result<Vec<Option<EntityId>>> {
        let mut results = vec![None; external_ids.len()];
        if self.runs.is_empty() || external_ids.is_empty() {
            return Ok(results);
        }
        let plain: Vec<RunDesc> = self.runs.iter().map(|e| e.desc.clone()).collect();
        let scan = scan_run_keys(&plain)?;

        // Sort input indices by key (not the keys themselves) so results can still be returned
        // in the caller's original order.
        let mut order: Vec<usize> = (0..external_ids.len()).collect();
        order.sort_by(|&a, &b| external_ids[a].cmp(&external_ids[b]));

        // Runs partition one ascending order (scan_run_keys already checked this), and `order`
        // visits keys ascending too, so the run cursor only ever moves forward — one pass,
        // each run opened at most once.
        let mut extent_idx = 0usize;
        for i in order {
            let key = &external_ids[i];
            while extent_idx < scan.bounds.len()
                && !scan.bounds[extent_idx]
                    .as_ref()
                    .is_some_and(|(_, last, _)| last.as_slice() >= key.as_slice())
            {
                extent_idx += 1;
            }
            if extent_idx >= scan.bounds.len() {
                // Past every run's last key: absent from the bundle, and so is every key
                // still to come (they only get larger) — but other, smaller-sorted keys already
                // resolved above may still be valid, so keep going rather than returning early.
                continue;
            }
            let run = self.runs[extent_idx].get_or_load()?;
            results[i] = run.resolve(key);
        }
        Ok(results)
    }

    /// The locator's length — the number of entity-id slots this bundle's build covered
    /// (`entity_id_high_water` at build time), or `0` if this deployment wrote no locator at all
    /// (no caller ever supplied an external id). Exposed so a caller (`tessera-engine`'s
    /// `Engine::external_id_of`, Important I-9) can tell "not covered by this bundle's locator"
    /// apart from "covered, and genuinely has no external id" **without** being handed the
    /// locator's path, digest or any other descriptor (Ruling B) — a plain count is none of
    /// those.
    pub fn locator_len(&self) -> u64 {
        self.locator.as_ref().map(|l| l.desc.len).unwrap_or(0)
    }

    /// `entity -> external_id`, distinguishing three outcomes instead of [`Self::external_id_of`]'s
    /// two (Important I-9): covered by this bundle's locator (delegates to
    /// [`Self::external_id_of`]); past the locator but below `high_water` (the live allocator
    /// high-water, supplied by the caller — this sidecar has no notion of anything ingested after
    /// build) with no live-map hit **is the caller's job to have already checked** — an
    /// inconsistency, not an absent external id, so this fails closed rather than returning a
    /// `None` that would read as "this item has no external id" for one that does; and beyond
    /// `high_water` entirely, which is `Ok(None)` — a post-build entity the live map doesn't know
    /// about genuinely has no external id.
    ///
    /// The `path` on the `Err` this can return is the locator's own — still never named by the
    /// caller (Ruling B holds: the caller passes only `entity` and `high_water`, never a path).
    pub fn external_id_of_checked(
        &self,
        entity: EntityId,
        high_water: u64,
    ) -> Result<Option<Vec<u8>>> {
        // A deployment that wrote no sidecar at all (contracts §2.4: callers supplied no
        // external IDs, so the build minted none — no runs, no locator). Every item's
        // identity is its `tessera_id` and `None` is the ordinary answer, not an inconsistency.
        // This state is unambiguous: `deferred_from_manifest` refuses a manifest whose runs
        // exist without a verifiable locator, so "no runs and no locator" can only mean the
        // build wrote none — a *lost* locator never reaches here as this state.
        if self.runs.is_empty() && self.locator.is_none() {
            return Ok(None);
        }
        if entity.raw() < self.locator_len() {
            return self.external_id_of(entity);
        }
        if entity.raw() < high_water {
            let path = self
                .locator
                .as_ref()
                .map(|l| l.desc.path.clone())
                .unwrap_or_else(|| PathBuf::from("<no locator — deployment wrote none>"));
            return Err(StoreError::InvalidSidecar {
                path,
                detail: format!(
                    "an entity below the live high-water ({high_water}) and past this bundle's \
                     locator ({} slots) is not known to the live external-id map — an \
                     inconsistency, not an absent external id",
                    self.locator_len(),
                ),
            });
        }
        Ok(None)
    }

    /// Resolve `entity` to its caller-supplied external id via the locator, or `Ok(None)` if
    /// `entity` has none (Ruling A: the ordinary case for an item whose identity is its
    /// `tessera_id`). A missing or corrupt locator, or a locator ordinal that doesn't fall in
    /// any run, is `Err(StoreError::InvalidSidecar)` — never a `None` that would read as "no
    /// external ID" for an item that in fact has one.
    pub fn external_id_of(&self, entity: EntityId) -> Result<Option<Vec<u8>>> {
        let Some(locator) = &self.locator else {
            return Ok(None);
        };
        let Some(ordinal) = locator.ordinal_of(entity)? else {
            return Ok(None);
        };

        let plain: Vec<RunDesc> = self.runs.iter().map(|e| e.desc.clone()).collect();
        let scan = scan_run_keys(&plain)?;

        let mut remaining = ordinal as u64;
        for (i, bounds) in scan.bounds.iter().enumerate() {
            let rows = bounds.as_ref().map(|(_, _, n)| *n as u64).unwrap_or(0);
            if remaining < rows {
                let run = self.runs[i].get_or_load()?;
                return Ok(Some(run.key(remaining as usize).to_vec()));
            }
            remaining -= rows;
        }

        Err(StoreError::InvalidSidecar {
            path: locator.desc.path.clone(),
            detail: "a locator ordinal exceeds the total external-id row count across all runs"
                .to_string(),
        })
    }
}

#[cfg(test)]
mod tests {
    use std::fs;

    use arrow::array::{ArrayRef, BinaryArray, UInt32Array};
    use arrow::datatypes::{DataType, Field, Schema};
    use arrow::ipc::writer::FileWriter;
    use arrow::record_batch::RecordBatch;
    use rand::rngs::StdRng;
    use rand::seq::SliceRandom;
    use rand::{Rng, SeedableRng};

    use super::*;

    /// Write one `external-ids-<n>.arrow`-shaped run from `rows` (not necessarily sorted —
    /// callers that want a fixture the build would actually produce pre-sort it themselves).
    fn write_extent(path: &Path, rows: &[(Vec<u8>, u32)]) {
        let schema = Arc::new(Schema::new(vec![
            Field::new("external_id", DataType::Binary, false),
            Field::new("entity_id", DataType::UInt32, false),
        ]));
        let ext: ArrayRef = Arc::new(BinaryArray::from_iter_values(
            rows.iter().map(|(id, _)| id.as_slice()),
        ));
        let ent: ArrayRef = Arc::new(UInt32Array::from_iter_values(rows.iter().map(|(_, e)| *e)));
        let batch = RecordBatch::try_new(schema.clone(), vec![ext, ent]).unwrap();
        let file = File::create(path).unwrap();
        let mut writer = FileWriter::try_new(file, &schema).unwrap();
        writer.write(&batch).unwrap();
        writer.finish().unwrap();
    }

    fn sha256_hex(path: &Path) -> String {
        let bytes = fs::read(path).unwrap();
        hex_digest(Sha256::digest(&bytes).as_slice())
    }

    fn desc(path: impl Into<PathBuf>, digest: impl Into<String>) -> RunDesc {
        RunDesc::new(path, digest)
    }

    /// Write a correctly sorted, correctly digested run at `path` and return its `RunDesc`.
    fn sorted_extent(dir: &Path, name: &str, rows: &[(Vec<u8>, u32)]) -> RunDesc {
        let mut sorted = rows.to_vec();
        sorted.sort_by(|a, b| a.0.cmp(&b.0));
        let path = dir.join(name);
        write_extent(&path, &sorted);
        desc(path.clone(), sha256_hex(&path))
    }

    /// Write a correctly-**digested** run whose rows are deliberately NOT in ascending order
    /// (Critical C-1's regression fixture): the digest matches the bytes on disk exactly, but a
    /// binary search over it would be unsound.
    fn write_extent_fixture_unsorted(
        rows: &[(&[u8], u32)],
    ) -> (tempfile::TempDir, PathBuf, String) {
        let dir = tempfile::TempDir::new().unwrap();
        let path = dir.path().join("external-ids-0.arrow");
        let owned: Vec<(Vec<u8>, u32)> = rows.iter().map(|(k, e)| (k.to_vec(), *e)).collect();
        write_extent(&path, &owned); // NOT sorted — written exactly as given.
        let digest = sha256_hex(&path);
        (dir, path, digest)
    }

    #[test]
    fn deferred_construction_opens_nothing() {
        // At Engine::open the sidecar must cost zero bytes of RSS and zero page faults. A path
        // that does not exist is the cheapest possible proof: if `deferred` opened, mapped or
        // verified anything, this would error instead of merely being unopened.
        let s = ExternalIdSidecar::deferred(vec![desc(
            "/nonexistent/external-ids-0.arrow",
            "0".repeat(64),
        )]);
        assert!(!s.is_open());
        assert_eq!(s.open_extents(), 0);
    }

    fn sidecar_with_three_extents(dir: &Path) -> (ExternalIdSidecar, Vec<[u8; 4]>) {
        // 30 distinct keys, globally sorted, split sequentially into 3 runs of 10 — mirrors
        // `tessera_build::write_external_id_runs`'s splitting rule.
        let mut keys: Vec<[u8; 4]> = (0..30u32).map(|i| (i * 7919).to_be_bytes()).collect();
        keys.sort();
        let mut descs = Vec::new();
        for (i, chunk) in keys.chunks(10).enumerate() {
            let rows: Vec<(Vec<u8>, u32)> = chunk
                .iter()
                .enumerate()
                .map(|(j, k)| (k.to_vec(), (i * 10 + j) as u32))
                .collect();
            descs.push(sorted_extent(
                dir,
                &format!("external-ids-{i}.arrow"),
                &rows,
            ));
        }
        (ExternalIdSidecar::deferred(descs), keys)
    }

    #[test]
    fn resolution_opens_one_extent_not_all_of_them() {
        // CRITICAL C-6: one OnceLock over ALL runs means the first drill-down maps and
        // digests the whole family, permanently. Laziness must be per run — the run is
        // selected by an O(runs) first-key/last-key scan that never opens (fully validates)
        // more than the one run the answer comes from.
        let dir = tempfile::TempDir::new().unwrap();
        let (sidecar, keys) = sidecar_with_three_extents(dir.path());
        let key_in_extent_1 = keys[15]; // runs are 0..10, 10..20, 20..30
        let found = sidecar.resolve(&key_in_extent_1).unwrap();
        assert_eq!(found, Some(EntityId::new(15)));
        assert_eq!(sidecar.open_extents(), 1);
        assert!(sidecar.is_open());
    }

    #[test]
    fn resolve_many_opens_each_extent_at_most_once_and_preserves_order() {
        // The point of batching: `/control/ingest`'s duplicate check calls this over a whole
        // batch, and must not open one run per row. All 30 keys span all three runs, so a
        // naive per-row `resolve` would open all three anyway here, but the assertion that
        // matters is that each opens EXACTLY once regardless of how many of its keys are queried
        // — repeat every key from run 1 many times over.
        let dir = tempfile::TempDir::new().unwrap();
        let (sidecar, keys) = sidecar_with_three_extents(dir.path());

        // Deliberately out of order and with repeats, and a query, to prove the function sorts
        // internally rather than requiring a sorted or deduplicated caller, and returns answers
        // in the CALLER's original order, not sorted order.
        let query: Vec<Vec<u8>> = vec![
            keys[25].to_vec(),           // run 2
            keys[5].to_vec(),            // run 0
            b"not-a-real-key!".to_vec(), // absent entirely
            keys[15].to_vec(),           // run 1
            keys[15].to_vec(),           // run 1 again
            keys[0].to_vec(),            // run 0, smallest key
        ];
        let results = sidecar.resolve_many(&query).unwrap();
        assert_eq!(
            results,
            vec![
                Some(EntityId::new(25)),
                Some(EntityId::new(5)),
                None,
                Some(EntityId::new(15)),
                Some(EntityId::new(15)),
                Some(EntityId::new(0)),
            ],
            "must preserve the caller's original order, not sorted order"
        );
        assert_eq!(
            sidecar.open_extents(),
            3,
            "every run that actually held a queried key opens exactly once, never once per row"
        );
    }

    #[test]
    fn resolve_many_on_an_empty_sidecar_answers_none_without_opening_anything() {
        let s = ExternalIdSidecar::deferred(vec![]);
        let results = s.resolve_many(&[b"a".to_vec(), b"b".to_vec()]).unwrap();
        assert_eq!(results, vec![None, None]);
        assert_eq!(s.open_extents(), 0);
    }

    #[test]
    fn an_out_of_order_extent_is_an_error_even_when_the_digest_matches() {
        // CRITICAL C-1: a digest does NOT subsume the sortedness check. The digest proves the
        // file is the one MANIFEST named; sortedness proves the binary search returns the right
        // answer. A build bug emitting an out-of-order run produces a correctly-digested
        // file, and a mis-resolved ID denies the wrong entity while leaving the intended target
        // visible.
        let (dir, path, digest) =
            write_extent_fixture_unsorted(&[(b"c", 3u32), (b"a", 1), (b"b", 2)]);
        let s = ExternalIdSidecar::deferred(vec![desc(path, digest)]);
        let err = s.resolve(b"b").unwrap_err();
        assert!(
            matches!(err, StoreError::InvalidSidecar { .. }),
            "got {err:?}"
        );
        drop(dir);
    }

    #[test]
    fn resolution_verifies_the_digest_and_fails_closed_on_corruption() {
        let dir = tempfile::TempDir::new().unwrap();
        let path = dir.path().join("external-ids-0.arrow");
        write_extent(&path, &[(vec![1, 2, 3, 4], 7u32)]);
        // Wrong digest — as if the file were corrupted or replaced after the manifest named it.
        let s = ExternalIdSidecar::deferred(vec![desc(path, "f".repeat(64))]);
        let err = s.resolve(&[1, 2, 3, 4]).unwrap_err();
        assert!(
            matches!(err, StoreError::InvalidSidecar { .. }),
            "got {err:?}"
        );
    }

    #[test]
    fn an_unknown_external_id_resolves_to_none_not_an_error() {
        let dir = tempfile::TempDir::new().unwrap();
        let (sidecar, _keys) = sidecar_with_three_extents(dir.path());
        assert_eq!(sidecar.resolve(&[0xAB; 4]).unwrap(), None);
        // A key past the last run's last key must also resolve to `None`, not error.
        assert_eq!(sidecar.resolve(&[0xFF; 4]).unwrap(), None);
    }

    #[test]
    fn a_missing_or_corrupt_locator_is_an_error_not_a_none() {
        // A `None` from the drill-down direction reads as "this item has no external ID", which
        // is a legitimate state. A corrupt locator must never be able to produce it.
        let dir = tempfile::TempDir::new().unwrap();
        let ext_path = dir.path().join("external-ids-0.arrow");
        write_extent(&ext_path, &[(vec![1, 2, 3, 4], 0u32)]);
        let ext_digest = sha256_hex(&ext_path);

        // No locator file at all at the expected path.
        let locator = LocatorDesc {
            path: dir.path().join("ext-locator.u32"),
            digest: "0".repeat(64),
            len: 1,
        };
        let s = ExternalIdSidecar::with_locator(vec![desc(ext_path, ext_digest)], locator);
        let err = s.external_id_of(EntityId::new(0)).unwrap_err();
        assert!(
            matches!(err, StoreError::InvalidSidecar { .. }),
            "got {err:?}"
        );
    }

    #[test]
    fn drill_down_round_trips_through_the_locator() {
        let dir = tempfile::TempDir::new().unwrap();
        // Two entities, 0 and 1; entity 1 has no external id (locator sentinel), entity 0's key
        // is "hello" — the locator's ordinal 0 into the single run's one row.
        let ext_path = dir.path().join("external-ids-0.arrow");
        write_extent(&ext_path, &[(b"hello".to_vec(), 0u32)]);
        let ext_digest = sha256_hex(&ext_path);

        let locator_path = dir.path().join("ext-locator.u32");
        let raw: Vec<u8> = [0u32, LOCATOR_NONE]
            .iter()
            .flat_map(|v| v.to_le_bytes())
            .collect();
        fs::write(&locator_path, &raw).unwrap();
        let locator_digest = sha256_hex(&locator_path);

        let s = ExternalIdSidecar::with_locator(
            vec![desc(ext_path, ext_digest)],
            LocatorDesc {
                path: locator_path,
                digest: locator_digest,
                len: 2,
            },
        );

        assert_eq!(
            s.external_id_of(EntityId::new(0)).unwrap(),
            Some(b"hello".to_vec())
        );
        assert_eq!(s.external_id_of(EntityId::new(1)).unwrap(), None);
        assert_eq!(s.resolve(b"hello").unwrap(), Some(EntityId::new(0)));
    }

    #[test]
    fn rejects_extents_out_of_order_relative_to_each_other() {
        // Each run is individually sorted ascending, but run 1's keys all precede run
        // 0's — a shuffled run *list*, not a shuffled run. Point 4: this must be validated,
        // not assumed, or a shuffled list silently resolves every lookup to `None`.
        let dir = tempfile::TempDir::new().unwrap();
        let d0 = sorted_extent(dir.path(), "external-ids-0.arrow", &[(vec![5, 0, 0, 0], 0)]);
        let d1 = sorted_extent(dir.path(), "external-ids-1.arrow", &[(vec![1, 0, 0, 0], 1)]);
        let s = ExternalIdSidecar::deferred(vec![d0, d1]);
        let err = s.resolve(&[1, 0, 0, 0]).unwrap_err();
        assert!(
            matches!(err, StoreError::InvalidSidecar { .. }),
            "got {err:?}"
        );
    }

    #[test]
    fn empty_extent_list_resolves_nothing_and_opens_nothing() {
        let s = ExternalIdSidecar::deferred(vec![]);
        assert_eq!(s.resolve(&[1, 2, 3]).unwrap(), None);
        assert_eq!(s.external_id_of(EntityId::new(0)).unwrap(), None);
        assert!(!s.is_open());
    }

    #[test]
    fn multi_extent_randomised_probes_match_a_naive_full_scan_oracle() {
        let dir = tempfile::TempDir::new().unwrap();
        let total = 400u32;
        let mut keys: Vec<[u8; 4]> = (0..total)
            .map(|i| (i.wrapping_mul(2654435761)).to_be_bytes())
            .collect();
        keys.sort();
        let mut descs = Vec::new();
        let mut oracle: Vec<(Vec<u8>, u32)> = Vec::new();
        for (i, chunk) in keys.chunks(37).enumerate() {
            let rows: Vec<(Vec<u8>, u32)> = chunk
                .iter()
                .enumerate()
                .map(|(j, k)| (k.to_vec(), (i * 37 + j) as u32))
                .collect();
            oracle.extend(rows.iter().cloned());
            descs.push(sorted_extent(
                dir.path(),
                &format!("external-ids-{i}.arrow"),
                &rows,
            ));
        }
        let sidecar = ExternalIdSidecar::deferred(descs);

        let mut rng = StdRng::seed_from_u64(0xC0FF_EE42);
        let mut present: Vec<[u8; 4]> = keys.clone();
        present.shuffle(&mut rng);
        for _ in 0..200 {
            let key: [u8; 4] = if rng.gen_bool(0.5) {
                *present.choose(&mut rng).unwrap()
            } else {
                rng.gen::<[u8; 4]>()
            };
            let expected = oracle
                .binary_search_by(|(k, _)| k.as_slice().cmp(&key))
                .ok()
                .map(|_| {
                    oracle
                        .iter()
                        .find(|(k, _)| k.as_slice() == key)
                        .map(|(_, e)| EntityId::new(*e as u64))
                        .unwrap()
                });
            assert_eq!(
                sidecar.resolve(&key).unwrap(),
                expected,
                "mismatch for {key:?}"
            );
        }
    }
}
