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
//! key has its `tessera_id` as its identifier, occupies no extent row, and carries a
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
//! extent the key falls in. At 10⁹ the previous eager mmap-and-linear-scan put 18.9 GB into the
//! resident set at `Engine::open` for a structure the per-viewport path never touches; a single
//! lock over the whole family would have restored most of that on the first click — laziness is
//! per **extent**, tracked by each extent's own [`OnceLock`], not one lock over the family.
//!
//! Integrity does *not* relax. A corrupted mapping suppresses the wrong item, so an extent's
//! digest **and its sortedness** are both verified before any answer comes out of it — a digest
//! proves the file is the one MANIFEST named, sortedness proves the binary search returns the
//! right answer, and a build bug emitting an out-of-order extent produces a correctly-digested
//! file. Every failure is a typed error ([`StoreError::InvalidSidecar`]), never a `None` that
//! would read as "unknown external ID".

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

/// One peeked extent's bounds: `(first_key, last_key, row_count)`.
type ExtentBounds = (Vec<u8>, Vec<u8>, usize);

/// One `external-ids-<n>.arrow` extent's identity: its path and the digest a well-formed bundle
/// records for it in a manifest's `files` map. **Internal to this module** — see the module doc
/// for why this must never leak into `tessera-engine` or `tessera-server`.
#[derive(Debug, Clone)]
struct ExtentDesc {
    path: PathBuf,
    /// Lowercase hex SHA-256, matching [`crate::manifest::FileDigest::sha256`].
    digest: String,
}

impl ExtentDesc {
    fn new(path: impl Into<PathBuf>, digest: impl Into<String>) -> Self {
        ExtentDesc {
            path: path.into(),
            digest: digest.into(),
        }
    }
}

/// The `entities/ext-locator.u32` file's identity plus its declared length (contracts §2.4 r6:
/// "length `entity_id_high_water` at build"). Internal, same reasoning as [`ExtentDesc`].
#[derive(Debug, Clone)]
struct LocatorDesc {
    path: PathBuf,
    digest: String,
    /// Entries — the file's length in `u32`s, i.e. `entity_id_high_water` at build.
    len: u64,
}

/// A digest-and-sortedness-verified extent, held zero-copy over its mmap (identical technique to
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

/// One extent's lazily-opened, per-extent state (Critical C-6). `cell` is populated on first
/// use and never again — a corrupt extent stays corrupt (typed error, cached as a message rather
/// than a live `StoreError` so this struct needs no `Clone` impl on `StoreError`'s I/O variant),
/// a valid one stays valid (the bundle contract makes every named file immutable once
/// published).
struct ExtentSlot {
    desc: ExtentDesc,
    cell: OnceLock<std::result::Result<ValidatedExtent, String>>,
}

impl ExtentSlot {
    fn new(desc: ExtentDesc) -> Self {
        ExtentSlot {
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

fn load_validated(desc: &ExtentDesc) -> std::result::Result<ValidatedExtent, String> {
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

/// R4 guarantees each extent is individually sorted ascending by external-id bytes. This is
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

/// A cheap, unvalidated peek at one extent's bounds: first key, last key, row count. Reads the
/// Arrow IPC footer and the two boundary values only — **no digest, no sortedness scan, no
/// caching** (Ruling B: nothing clever). Used purely to select which single extent a lookup
/// falls in and to validate the extent *list's* ordering (Critical C-6/point 4); the extent
/// actually resolved into is always separately, fully validated by [`ExtentSlot::get_or_load`]
/// before an answer is returned. `Ok(None)` means the extent has zero rows.
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

/// One extent's peeked bounds (or `None` if empty), paired with its resident, digest-verified
/// full open on demand.
struct BoundsScan {
    /// Per-extent: `Some((first_key, last_key, row_count))`, or `None` if that extent is empty.
    bounds: Vec<Option<ExtentBounds>>,
}

/// Peek every extent's bounds, in order, validating that non-empty extents partition one
/// ascending order across the whole family (Critical: cross-extent ordering must be validated,
/// not assumed — point 4). Fresh on every call: no cache (Ruling B).
fn scan_bounds(descs: &[ExtentDesc]) -> Result<BoundsScan> {
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
                            "extent {prev_idx}'s last key is not strictly less than extent \
                             {idx}'s first key — extents must partition one ascending order"
                        ),
                    });
                }
            }
            prev_non_empty = Some((idx, last.clone()));
        }
        bounds.push(this);
    }
    Ok(BoundsScan { bounds })
}

/// The lazily-opened `entities/ext-locator.u32` sidecar: one raw `u32` array, no header, length
/// `len` (`entity_id_high_water` at build), `locator[entity_id]` gives that entity's ordinal in
/// the concatenated sorted external-id extents, or [`LOCATOR_NONE`] if the entity has no
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

    /// This entity's ordinal into the concatenated external-id extents, or `None` if it has no
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
    extents: Vec<ExtentSlot>,
    locator: Option<LocatorSlot>,
}

impl ExternalIdSidecar {
    /// Build a sidecar over `extents`, none of which are opened, mapped or verified by this call
    /// (Critical C-6's laziness starts here). No locator — `external_id_of` always returns
    /// `Ok(None)`. Exposed for this module's own tests; the type held by [`ExtentDesc`] is
    /// private to this crate, so this constructor cannot be called, and no extent descriptor can
    /// be named, from `tessera-engine` or `tessera-server`.
    fn deferred(extents: Vec<ExtentDesc>) -> Self {
        ExternalIdSidecar {
            extents: extents.into_iter().map(ExtentSlot::new).collect(),
            locator: None,
        }
    }

    fn with_locator(extents: Vec<ExtentDesc>, locator: LocatorDesc) -> Self {
        ExternalIdSidecar {
            extents: extents.into_iter().map(ExtentSlot::new).collect(),
            locator: Some(LocatorSlot::new(locator)),
        }
    }

    /// The public constructor: build the sidecar directly from the bundle's top-level manifest,
    /// the partition's side-manifest, and the bundle's prefix directory. **This is the only
    /// thing `Engine::open` calls** — no `ExtentDesc`, digest, ordinal or file path is handed
    /// back to the caller; replacing the storage behind this sidecar is a change to this file
    /// plus this constructor's call site, nothing else (Ruling B's acceptance test).
    ///
    /// A deployment whose callers supplied no external IDs writes no extents and no locator at
    /// all (contracts §2.4 r6) — `external_id_extents` empty is not an error, it degenerates to
    /// a sidecar that always answers `Ok(None)` in both directions.
    pub fn deferred_from_manifest(
        bundle_manifest: &Manifest,
        partition_manifest: &SegmentsManifest,
        prefix_dir: &Path,
    ) -> Result<Self> {
        let mut extents = Vec::with_capacity(partition_manifest.external_id_extents.len());
        for rel in &partition_manifest.external_id_extents {
            let digest = partition_manifest
                .files
                .get(rel)
                .or_else(|| bundle_manifest.files.get(rel))
                .ok_or_else(|| StoreError::UnverifiedFile {
                    path: prefix_dir.join(rel),
                })?;
            extents.push(ExtentDesc::new(prefix_dir.join(rel), digest.sha256.clone()));
        }

        if extents.is_empty() {
            return Ok(Self::deferred(extents));
        }

        // The locator has no dedicated manifest field (contracts §2.4 r6 gives it a fixed name,
        // no `<k>` suffix); it always lives alongside the extents in the same `entities/`
        // directory, so its prefix-relative path is derived from theirs rather than assumed from
        // a partition hash this constructor deliberately does not need to know.
        let first_rel = &partition_manifest.external_id_extents[0];
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

        Ok(Self::with_locator(extents, locator))
    }

    /// `true` if at least one extent (or the locator) has been opened.
    pub fn is_open(&self) -> bool {
        self.open_extents() > 0 || self.locator.as_ref().is_some_and(|l| l.is_open())
    }

    /// How many extents have been fully opened (mapped, digest- and sortedness-verified) so far
    /// — for the residency test and Task 15's memo, which must report steady state as *base +
    /// one extent*, not the whole family.
    pub fn open_extents(&self) -> usize {
        self.extents.iter().filter(|e| e.is_open()).count()
    }

    /// Resolve `external_id` to its entity id, or `Ok(None)` if it names nothing in this
    /// sidecar. Every failure — corrupt extent, out-of-order extent, mismatched digest, a
    /// shuffled extent list — is `Err(StoreError::InvalidSidecar)`, never folded into `Ok(None)`
    /// (a fail-closed control-plane caller must not read "corrupt" as "not found").
    pub fn resolve(&self, external_id: &[u8]) -> Result<Option<EntityId>> {
        if self.extents.is_empty() {
            return Ok(None);
        }
        let plain: Vec<ExtentDesc> = self.extents.iter().map(|e| e.desc.clone()).collect();
        let scan = scan_bounds(&plain)?;

        let Some(idx) = scan.bounds.iter().position(|b| {
            b.as_ref()
                .is_some_and(|(_, last, _)| last.as_slice() >= external_id)
        }) else {
            return Ok(None);
        };

        let extent = self.extents[idx].get_or_load()?;
        Ok(extent.resolve(external_id))
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
        if entity.raw() < self.locator_len() {
            return self.external_id_of(entity);
        }
        if entity.raw() < high_water {
            let path = self
                .locator
                .as_ref()
                .map(|l| l.desc.path.clone())
                .unwrap_or_else(|| PathBuf::from("<no locator extent — deployment wrote none>"));
            return Err(StoreError::InvalidSidecar {
                path,
                detail: format!(
                    "entity {} is below the live high-water ({high_water}) and past this \
                     bundle's locator ({} slots), but is not known to the live external-id map — \
                     an inconsistency, not an absent external id",
                    entity.raw(),
                    self.locator_len(),
                ),
            });
        }
        Ok(None)
    }

    /// Resolve `entity` to its caller-supplied external id via the locator, or `Ok(None)` if
    /// `entity` has none (Ruling A: the ordinary case for an item whose identity is its
    /// `tessera_id`). A missing or corrupt locator, or a locator ordinal that doesn't fall in
    /// any extent, is `Err(StoreError::InvalidSidecar)` — never a `None` that would read as "no
    /// external ID" for an item that in fact has one.
    pub fn external_id_of(&self, entity: EntityId) -> Result<Option<Vec<u8>>> {
        let Some(locator) = &self.locator else {
            return Ok(None);
        };
        let Some(ordinal) = locator.ordinal_of(entity)? else {
            return Ok(None);
        };

        let plain: Vec<ExtentDesc> = self.extents.iter().map(|e| e.desc.clone()).collect();
        let scan = scan_bounds(&plain)?;

        let mut remaining = ordinal as u64;
        for (i, bounds) in scan.bounds.iter().enumerate() {
            let rows = bounds.as_ref().map(|(_, _, n)| *n as u64).unwrap_or(0);
            if remaining < rows {
                let extent = self.extents[i].get_or_load()?;
                return Ok(Some(extent.key(remaining as usize).to_vec()));
            }
            remaining -= rows;
        }

        Err(StoreError::InvalidSidecar {
            path: locator.desc.path.clone(),
            detail: format!(
                "locator ordinal {ordinal} for entity {} exceeds the total external-id row \
                 count across all extents",
                entity.raw()
            ),
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

    /// Write one `external-ids-<n>.arrow`-shaped extent from `rows` (not necessarily sorted —
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

    fn desc(path: impl Into<PathBuf>, digest: impl Into<String>) -> ExtentDesc {
        ExtentDesc::new(path, digest)
    }

    /// Write a correctly sorted, correctly digested extent at `path` and return its `ExtentDesc`.
    fn sorted_extent(dir: &Path, name: &str, rows: &[(Vec<u8>, u32)]) -> ExtentDesc {
        let mut sorted = rows.to_vec();
        sorted.sort_by(|a, b| a.0.cmp(&b.0));
        let path = dir.join(name);
        write_extent(&path, &sorted);
        desc(path.clone(), sha256_hex(&path))
    }

    /// Write a correctly-**digested** extent whose rows are deliberately NOT in ascending order
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
        // 30 distinct keys, globally sorted, split sequentially into 3 extents of 10 — mirrors
        // `tessera_build::write_external_id_extents`'s splitting rule.
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
        // CRITICAL C-6: one OnceLock over ALL extents means the first drill-down maps and
        // digests the whole family, permanently. Laziness must be per extent — the extent is
        // selected by an O(extents) first-key/last-key scan that never opens (fully validates)
        // more than the one extent the answer comes from.
        let dir = tempfile::TempDir::new().unwrap();
        let (sidecar, keys) = sidecar_with_three_extents(dir.path());
        let key_in_extent_1 = keys[15]; // extents are 0..10, 10..20, 20..30
        let found = sidecar.resolve(&key_in_extent_1).unwrap();
        assert_eq!(found, Some(EntityId::new(15)));
        assert_eq!(sidecar.open_extents(), 1);
        assert!(sidecar.is_open());
    }

    #[test]
    fn an_out_of_order_extent_is_an_error_even_when_the_digest_matches() {
        // CRITICAL C-1: a digest does NOT subsume the sortedness check. The digest proves the
        // file is the one MANIFEST named; sortedness proves the binary search returns the right
        // answer. A build bug emitting an out-of-order extent produces a correctly-digested
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
        // A key past the last extent's last key must also resolve to `None`, not error.
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
        // is "hello" — the locator's ordinal 0 into the single extent's one row.
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
        // Each extent is individually sorted ascending, but extent 1's keys all precede extent
        // 0's — a shuffled extent *list*, not a shuffled extent. Point 4: this must be validated,
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
