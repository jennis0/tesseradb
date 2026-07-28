//! The bundle read protocol (contracts §2.3), the zero-copy `columns.arrow` / `morton.u64`
//! loader, and `tile_ranges`.
//!
//! `tessera-store` never depends on `tessera-authz`, and this module has its own Arrow IPC
//! reader — `columns.arrow`'s schema (fixed-width primitive columns) differs from
//! `tessera-authz::postings`'s single `LargeBinary` column, so the zero-copy technique (mmap →
//! `arrow::buffer::Buffer::from_custom_allocation` → decode without copying) is reused, not the
//! code.

use std::collections::{BTreeMap, HashMap};
use std::fs::File;
use std::ops::Range;
use std::path::Path;
use std::ptr::NonNull;
use std::sync::Arc;

use arrow::array::{Array, Float32Array, StringArray, UInt16Array, UInt32Array, UInt64Array};
use arrow::buffer::Buffer;
use arrow::datatypes::{DataType, SchemaRef};
use arrow::ipc::convert::fb_to_schema;
use arrow::ipc::reader::{read_footer_length, FileDecoder};
use arrow::ipc::{root_as_footer, root_as_message, Block, MetadataVersion};
use arrow::record_batch::RecordBatch;
use memmap2::Mmap;
use sha2::{Digest, Sha256};

use tessera_spatial::Tile;
use tessera_types::BUNDLE_FORMAT;

use crate::error::{read_to_vec, Result, StoreError};
use crate::manifest::{CurrentPointer, FileDigest, Manifest, SegmentsManifest};
use crate::permutation::Permutation;

/// One loaded (partition, slice) pair: the permutation addressing its rows, and every segment
/// in that slice — Phase 1 always has exactly one (contracts §2.1's "one segment per
/// (partition, slice) at build"); the field is a `Vec` because the on-disk shape (and the
/// engine's tile-lookup interface, per the task brief) already generalises to streamed
/// segments.
#[derive(Debug)]
pub struct SliceData {
    pub permutation: Permutation,
    pub segments: Vec<SegmentData>,
}

/// One loaded segment: its row count, its Morton codes (row order, ascending), and a zero-copy
/// view into its `columns.arrow`.
#[derive(Debug)]
pub struct SegmentData {
    pub seg_id: String,
    pub row_count: u32,
    pub morton: MortonSlice,
    pub columns: ColumnsRef,
}

/// One loaded partition: its verified side-manifest and every slice it names.
#[derive(Debug)]
pub struct PartitionData {
    pub manifest: SegmentsManifest,
    pub slices: HashMap<String, SliceData>,
}

/// An open, digest-verified bundle: the top-level manifest plus every partition's loaded data.
#[derive(Debug)]
pub struct Bundle {
    pub manifest: Manifest,
    pub partitions: HashMap<String, PartitionData>,
}

/// Open `root` (a bundle directory containing `CURRENT`) following the read protocol
/// (contracts §2.3): `CURRENT` → digest-checked `MANIFEST.json` (refusing `bundle_format` newer
/// than this reader) → per partition, the highest `SEGMENTS-<n>.json` whose listed files (and
/// `MANIFEST.json`'s) all verify by size and SHA-256, stepping down on failure. Any failure at
/// any stage is a typed error — fail-closed, per the invariant this method exists to uphold:
/// serving must never treat a partially-verified bundle as ready.
pub fn open_bundle(root: &Path) -> Result<Bundle> {
    let current_path = root.join("CURRENT");
    let current_bytes = read_to_vec(&current_path)?;
    let current: CurrentPointer =
        serde_json::from_slice(&current_bytes).map_err(|source| StoreError::Json {
            path: current_path.clone(),
            source,
        })?;

    let prefix_dir = root.join(&current.prefix);
    let manifest_path = prefix_dir.join("MANIFEST.json");
    let manifest_bytes = read_to_vec(&manifest_path)?;

    let actual_digest = hex_sha256(&manifest_bytes);
    if actual_digest != current.manifest_digest {
        return Err(StoreError::ManifestDigestMismatch {
            expected: current.manifest_digest,
            actual: actual_digest,
        });
    }

    let manifest: Manifest =
        serde_json::from_slice(&manifest_bytes).map_err(|source| StoreError::Json {
            path: manifest_path.clone(),
            source,
        })?;

    if manifest.bundle_format > BUNDLE_FORMAT {
        return Err(StoreError::UnsupportedBundleFormat {
            found: manifest.bundle_format,
            max_supported: BUNDLE_FORMAT,
        });
    }

    // The MANIFEST-level `files` set (dictionary extents and anything else it names) is
    // verified once, up front — it isn't partition-specific, and the reader protocol requires
    // every one of these entries to verify regardless of which SEGMENTS-<n>.json a partition
    // settles on.
    verify_files(&prefix_dir, &manifest.files)?;

    let mut partitions = HashMap::with_capacity(manifest.partitions.len());
    for partition_desc in &manifest.partitions {
        let partition_dir = prefix_dir.join("partitions").join(&partition_desc.phash);
        let segments_manifest = load_verifying_segments_manifest(&prefix_dir, &partition_dir)?;

        let mut slices: HashMap<String, SliceData> = HashMap::new();
        for seg_desc in &segments_manifest.segments {
            let slice_dir = partition_dir.join("slices").join(&seg_desc.slice);
            let slice_entry = match slices.get_mut(&seg_desc.slice) {
                Some(entry) => entry,
                None => {
                    let permutation = Permutation::load(&slice_dir.join("permutation.bin"))?;
                    slices.insert(
                        seg_desc.slice.clone(),
                        SliceData {
                            permutation,
                            segments: Vec::new(),
                        },
                    );
                    slices.get_mut(&seg_desc.slice).expect("just inserted")
                }
            };

            let seg_dir = slice_dir.join("segments").join(&seg_desc.seg_id);
            let morton = MortonSlice::load(&seg_dir.join("morton.u64"))?;
            let columns = ColumnsRef::load(&seg_dir.join("columns.arrow"))?;

            if morton.len() as u32 != seg_desc.row_count
                || columns.row_count() != seg_desc.row_count
            {
                return Err(StoreError::MalformedBundle {
                    detail: format!(
                        "segment '{}' (slice '{}'): manifest row_count {} doesn't match \
                         morton.u64 ({} codes) or columns.arrow ({} rows)",
                        seg_desc.seg_id,
                        seg_desc.slice,
                        seg_desc.row_count,
                        morton.len(),
                        columns.row_count()
                    ),
                });
            }

            slice_entry.segments.push(SegmentData {
                seg_id: seg_desc.seg_id.clone(),
                row_count: seg_desc.row_count,
                morton,
                columns,
            });
        }

        partitions.insert(
            partition_desc.phash.clone(),
            PartitionData {
                manifest: segments_manifest,
                slices,
            },
        );
    }

    Ok(Bundle {
        manifest,
        partitions,
    })
}

/// Find the highest-numbered `SEGMENTS-<n>.json` under `partition_dir` whose own `files` all
/// verify by size and SHA-256, stepping down through lower `n` on failure. Errors (fail-closed)
/// if none verify.
// `SEGMENTS-<n>.json`'s own `files` map, like `MANIFEST.json`'s, is keyed by paths relative to
// the bundle *prefix* directory (R1: "manifest paths prefix-relative"), not to the partition
// directory the side-manifest itself lives in — so verification is against `prefix_dir`, even
// though the side-manifest file is found by walking `partition_dir`.
fn load_verifying_segments_manifest(
    prefix_dir: &Path,
    partition_dir: &Path,
) -> Result<SegmentsManifest> {
    let mut candidates = list_segments_manifests(partition_dir)?;
    // Highest n first.
    candidates.sort_unstable_by(|a, b| b.cmp(a));

    let partition_label = partition_dir
        .file_name()
        .map(|s| s.to_string_lossy().into_owned())
        .unwrap_or_else(|| partition_dir.display().to_string());

    for n in candidates {
        let path = partition_dir.join(format!("SEGMENTS-{n}.json"));
        let Ok(bytes) = std::fs::read(&path) else {
            continue;
        };
        let Ok(segments_manifest) = serde_json::from_slice::<SegmentsManifest>(&bytes) else {
            continue;
        };
        if verify_files(prefix_dir, &segments_manifest.files).is_ok() {
            return Ok(segments_manifest);
        }
    }

    Err(StoreError::NoVerifyingSegmentsManifest {
        partition: partition_label,
    })
}

/// List the `n` values of every `SEGMENTS-<n>.json` present in `partition_dir` (unordered,
/// unverified — candidates only).
fn list_segments_manifests(partition_dir: &Path) -> Result<Vec<u64>> {
    let entries = match std::fs::read_dir(partition_dir) {
        Ok(entries) => entries,
        // No such directory at all is not itself a hard read error here: the caller reports a
        // typed "no verifying manifest" error either way, with a clearer message than a raw
        // ENOENT would give.
        Err(_) => return Ok(Vec::new()),
    };

    let mut found = Vec::new();
    for entry in entries {
        let entry = entry.map_err(|source| StoreError::Io {
            path: partition_dir.to_path_buf(),
            source,
        })?;
        let name = entry.file_name();
        let name = name.to_string_lossy();
        if let Some(rest) = name
            .strip_prefix("SEGMENTS-")
            .and_then(|r| r.strip_suffix(".json"))
        {
            if let Ok(n) = rest.parse::<u64>() {
                found.push(n);
            }
        }
    }
    Ok(found)
}

/// Verify every entry of `files` (path relative to `base`, forward slashes per R1) by exact
/// size and SHA-256 hex digest. Any missing, mis-sized or mismatched file is a hard error.
fn verify_files(base: &Path, files: &BTreeMap<String, FileDigest>) -> Result<()> {
    for (rel_path, digest) in files {
        let path = base.join(rel_path.replace('/', std::path::MAIN_SEPARATOR_STR));
        let bytes = std::fs::read(&path).map_err(|source| StoreError::Io {
            path: path.clone(),
            source,
        })?;
        if bytes.len() as u64 != digest.size {
            return Err(StoreError::FileVerificationFailed {
                path,
                reason: format!(
                    "size mismatch: manifest says {}, file is {} bytes",
                    digest.size,
                    bytes.len()
                ),
            });
        }
        let actual = hex_sha256(&bytes);
        if actual != digest.sha256 {
            return Err(StoreError::FileVerificationFailed {
                path,
                reason: format!(
                    "SHA-256 mismatch: manifest says {}, computed {actual}",
                    digest.sha256
                ),
            });
        }
    }
    Ok(())
}

fn hex_sha256(bytes: &[u8]) -> String {
    let digest = Sha256::digest(bytes);
    let mut out = String::with_capacity(digest.len() * 2);
    for byte in digest {
        out.push_str(&format!("{byte:02x}"));
    }
    out
}

/// A memory-mapped, zero-copy view of `morton.u64`: raw sorted little-endian `u64` codes, no
/// header (R4).
#[derive(Debug)]
pub struct MortonSlice {
    mmap: Mmap,
}

impl MortonSlice {
    pub fn load(path: &Path) -> Result<Self> {
        let file = File::open(path).map_err(|source| StoreError::Io {
            path: path.to_path_buf(),
            source,
        })?;
        // SAFETY: read-only for this struct's lifetime; see `Permutation::load`'s note on the
        // shared operational hazard of a concurrently-truncated backing file.
        let mmap = unsafe { Mmap::map(&file) }.map_err(|source| StoreError::Io {
            path: path.to_path_buf(),
            source,
        })?;
        if mmap.len() % 8 != 0 {
            return Err(StoreError::MalformedBundle {
                detail: format!(
                    "{}: length {} is not a multiple of 8",
                    path.display(),
                    mmap.len()
                ),
            });
        }
        Ok(MortonSlice { mmap })
    }

    /// The number of codes (rows) in this segment.
    pub fn len(&self) -> usize {
        self.mmap.len() / 8
    }

    pub fn is_empty(&self) -> bool {
        self.mmap.is_empty()
    }

    /// The codes, in row order (ascending, ties broken by priority then entity ID at write
    /// time — contracts §2.6).
    pub fn u64(&self) -> &[u64] {
        // SAFETY: length is a checked multiple of 8 (validated at `load`); the mmap base is
        // page-aligned (>= 8-byte aligned) by construction, so this cast is always valid — no
        // per-open re-check needed the way `permutation.bin`'s offset-16 slice needed one,
        // since here the slice starts at offset 0.
        unsafe { std::slice::from_raw_parts(self.mmap.as_ptr() as *const u64, self.len()) }
    }
}

/// One declared-scalar column's typed, zero-copy value slice.
#[derive(Debug)]
pub enum ScalarSlice<'a> {
    U64(&'a [u64]),
    F32(&'a [f32]),
    /// Variable-length; `StringArray` itself is a zero-copy view over the mapped buffers, so
    /// this is still zero-copy even though it isn't a flat `&[&str]`.
    Utf8(&'a StringArray),
}

/// A zero-copy, mmap-backed view of `columns.arrow`. Validated once at [`ColumnsRef::load`]:
/// exactly one record batch, uncompressed, 8-byte-aligned buffers (via
/// [`FileDecoder::with_require_alignment`]), and the five fixed columns present with the
/// expected names and types (R4). Every accessor below borrows directly from the underlying
/// `RecordBatch`'s buffers — no per-call copy.
#[derive(Debug)]
pub struct ColumnsRef {
    batch: RecordBatch,
    scalar_index: HashMap<String, usize>,
}

const FIXED_COLUMNS: [(&str, DataType); 5] = [
    ("entity_id", DataType::UInt64),
    ("x", DataType::Float32),
    ("y", DataType::Float32),
    ("node_id", DataType::UInt32),
    ("priority", DataType::UInt16),
];

impl ColumnsRef {
    pub fn load(path: &Path) -> Result<Self> {
        let file = File::open(path).map_err(|source| StoreError::Io {
            path: path.to_path_buf(),
            source,
        })?;
        // SAFETY: identical justification to `tessera_authz::postings::PostingsReader::open`'s
        // mmap branch — `arc` outlives every `Buffer` built from it (captured as the buffer's
        // `Allocation`), the mapping is valid for `len` bytes for its whole lifetime, and
        // `memmap2::Mmap` never returns a null base pointer.
        let mapping = unsafe { Mmap::map(&file) }.map_err(|source| StoreError::Io {
            path: path.to_path_buf(),
            source,
        })?;
        let len = mapping.len();
        let arc: Arc<Mmap> = Arc::new(mapping);
        let ptr = NonNull::new(arc.as_ptr() as *mut u8)
            .expect("memmap2::Mmap never returns a null base pointer");
        let buffer = unsafe { Buffer::from_custom_allocation(ptr, len, arc) };

        let batch = decode_single_batch(&buffer, path)?;
        validate_schema(&batch, path)?;

        let scalar_index = batch
            .schema_ref()
            .fields()
            .iter()
            .enumerate()
            .skip(FIXED_COLUMNS.len())
            .map(|(idx, field)| (field.name().clone(), idx))
            .collect();

        Ok(ColumnsRef {
            batch,
            scalar_index,
        })
    }

    pub fn row_count(&self) -> u32 {
        self.batch.num_rows() as u32
    }

    pub fn entity_id(&self) -> &[u64] {
        downcast::<UInt64Array>(&self.batch, 0).values()
    }

    pub fn x(&self) -> &[f32] {
        downcast::<Float32Array>(&self.batch, 1).values()
    }

    pub fn y(&self) -> &[f32] {
        downcast::<Float32Array>(&self.batch, 2).values()
    }

    pub fn node_id(&self) -> &[u32] {
        downcast::<UInt32Array>(&self.batch, 3).values()
    }

    pub fn priority(&self) -> &[u16] {
        downcast::<UInt16Array>(&self.batch, 4).values()
    }

    /// A declared-scalar column by name, or `None` if `columns.arrow` has no such column.
    pub fn scalar(&self, name: &str) -> Option<ScalarSlice<'_>> {
        let idx = *self.scalar_index.get(name)?;
        let column = self.batch.column(idx);
        Some(match column.data_type() {
            DataType::UInt64 => ScalarSlice::U64(
                column
                    .as_any()
                    .downcast_ref::<UInt64Array>()
                    .expect("data_type checked")
                    .values(),
            ),
            DataType::Float32 => ScalarSlice::F32(
                column
                    .as_any()
                    .downcast_ref::<Float32Array>()
                    .expect("data_type checked")
                    .values(),
            ),
            DataType::Utf8 => ScalarSlice::Utf8(
                column
                    .as_any()
                    .downcast_ref::<StringArray>()
                    .expect("data_type checked"),
            ),
            other => panic!(
                "columns.arrow: scalar '{name}' has an unsupported type {other:?} that \
                 should have been rejected at load"
            ),
        })
    }
}

/// Downcast column `idx` of `batch` to `T` (one of the fixed-column array types), panicking on
/// mismatch — safe to assume because [`validate_schema`] already checked every fixed column's
/// type at `ColumnsRef::load`, before any `ColumnsRef` accessor is reachable.
fn downcast<T: Array + 'static>(batch: &RecordBatch, idx: usize) -> &T {
    batch
        .column(idx)
        .as_any()
        .downcast_ref::<T>()
        .expect("validated at ColumnsRef::load")
}

fn validate_schema(batch: &RecordBatch, path: &Path) -> Result<()> {
    let schema = batch.schema_ref();
    if schema.fields().len() < FIXED_COLUMNS.len() {
        return Err(StoreError::InvalidColumns {
            path: path.to_path_buf(),
            detail: format!(
                "expected at least {} columns, found {}",
                FIXED_COLUMNS.len(),
                schema.fields().len()
            ),
        });
    }
    for (idx, (name, ty)) in FIXED_COLUMNS.iter().enumerate() {
        let field = schema.field(idx);
        if field.name() != name || field.data_type() != ty {
            return Err(StoreError::InvalidColumns {
                path: path.to_path_buf(),
                detail: format!(
                    "column {idx}: expected '{name}' ({ty:?}), found '{}' ({:?})",
                    field.name(),
                    field.data_type()
                ),
            });
        }
    }
    for field in schema.fields().iter().skip(FIXED_COLUMNS.len()) {
        if !matches!(
            field.data_type(),
            DataType::UInt64 | DataType::Float32 | DataType::Utf8
        ) {
            return Err(StoreError::InvalidColumns {
                path: path.to_path_buf(),
                detail: format!(
                    "declared scalar '{}' has unsupported type {:?}",
                    field.name(),
                    field.data_type()
                ),
            });
        }
    }
    Ok(())
}

/// Decode the (single, uncompressed, 8-byte-aligned) record batch of an Arrow IPC FILE held in
/// `buffer`. Structurally the same footer/dictionary/block walk as
/// `tessera_authz::postings::decode_single_batch`, generalised to `columns.arrow`'s schema and
/// hardened with two checks that file has no need of: `with_require_alignment(true)` (fail
/// closed on a misaligned buffer rather than silently reallocating) and an explicit rejection
/// of compressed batches (§ "no compression" in the task brief — decoding would otherwise
/// quietly succeed via an allocated, decompressed copy, defeating the zero-copy contract
/// without ever raising an error).
fn decode_single_batch(buffer: &Buffer, path: &Path) -> Result<RecordBatch> {
    const FOOTER_TRAILER_LEN: usize = 10; // 4-byte footer length + 6-byte "ARROW1" magic
    if buffer.len() < FOOTER_TRAILER_LEN {
        return Err(invalid_columns(path, "file too short to contain a footer"));
    }

    let trailer_start = buffer.len() - FOOTER_TRAILER_LEN;
    let trailer: [u8; FOOTER_TRAILER_LEN] = buffer[trailer_start..]
        .try_into()
        .expect("slice length matches FOOTER_TRAILER_LEN");
    let footer_len = read_footer_length(trailer)
        .map_err(|e| invalid_columns(path, &format!("bad footer length: {e}")))?;
    if footer_len > trailer_start {
        return Err(invalid_columns(path, "footer length exceeds file size"));
    }

    let footer = root_as_footer(&buffer[trailer_start - footer_len..trailer_start])
        .map_err(|e| invalid_columns(path, &format!("invalid footer: {e}")))?;

    if footer.dictionaries().map(|d| d.len()).unwrap_or(0) != 0 {
        return Err(invalid_columns(
            path,
            "dictionary-encoded columns are not supported",
        ));
    }

    let schema_fb = footer
        .schema()
        .ok_or_else(|| invalid_columns(path, "footer has no schema"))?;
    let schema: SchemaRef = Arc::new(fb_to_schema(schema_fb));

    let version: MetadataVersion = footer.version();
    let decoder = FileDecoder::new(schema, version).with_require_alignment(true);

    let batches = footer
        .recordBatches()
        .ok_or_else(|| invalid_columns(path, "footer has no record batches"))?;
    if batches.len() != 1 {
        return Err(invalid_columns(
            path,
            &format!("expected exactly one record batch, found {}", batches.len()),
        ));
    }

    let block = batches.get(0);
    let (offset, body_len, meta_len) = checked_block_range(path, block, buffer.len())?;
    let data = buffer.slice_with_length(offset, body_len + meta_len);

    reject_if_compressed(path, &data, meta_len)?;

    decoder
        .read_record_batch(block, &data)
        .map_err(|e| invalid_columns(path, &format!("{e}")))?
        .ok_or_else(|| invalid_columns(path, "record batch block decoded to nothing"))
}

const CONTINUATION_MARKER: [u8; 4] = [0xff, 0xff, 0xff, 0xff];

/// Parse the IPC `Message` metadata prefix of a record-batch block and reject it outright if
/// its body is compressed. This duplicates a small slice of what `FileDecoder::read_record_batch`
/// parses internally — there is no public API to ask "was this compressed?" after the fact,
/// and by the time the batch is decoded a compressed buffer has already been silently
/// reallocated into an owned, decompressed copy.
fn reject_if_compressed(path: &Path, data: &Buffer, meta_len: usize) -> Result<()> {
    if meta_len > data.len() {
        return Err(invalid_columns(
            path,
            "block metaDataLength exceeds block data",
        ));
    }
    let meta = &data[..meta_len];
    if meta.len() < 4 {
        return Err(invalid_columns(path, "message metadata too short"));
    }
    let stripped = if meta[..4] == CONTINUATION_MARKER {
        if meta.len() < 8 {
            return Err(invalid_columns(path, "message metadata too short"));
        }
        &meta[8..]
    } else {
        &meta[4..]
    };
    let message = root_as_message(stripped)
        .map_err(|e| invalid_columns(path, &format!("invalid message metadata: {e}")))?;
    if let Some(record_batch) = message.header_as_record_batch() {
        if record_batch.compression().is_some() {
            return Err(invalid_columns(
                path,
                "compressed record batches are not supported (uncompressed buffers only, R4)",
            ));
        }
    }
    Ok(())
}

/// Validate a footer `Block`'s `(offset, bodyLength, metaDataLength)` against the file length,
/// returning them as checked `usize`s (see `tessera_authz::postings::checked_block_range` for
/// the identical rationale: `Block`'s fields are `i64` in the flatbuffer schema, and
/// `Buffer::slice_with_length` panics on out-of-bounds input rather than erroring).
fn checked_block_range(
    path: &Path,
    block: &Block,
    buffer_len: usize,
) -> Result<(usize, usize, usize)> {
    let offset = usize::try_from(block.offset())
        .map_err(|_| invalid_columns(path, "block offset is negative"))?;
    let body_len = usize::try_from(block.bodyLength())
        .map_err(|_| invalid_columns(path, "block bodyLength is negative"))?;
    let meta_len = usize::try_from(block.metaDataLength())
        .map_err(|_| invalid_columns(path, "block metaDataLength is negative"))?;
    let total = body_len
        .checked_add(meta_len)
        .ok_or_else(|| invalid_columns(path, "block length overflows"))?;
    let end = offset
        .checked_add(total)
        .ok_or_else(|| invalid_columns(path, "block offset + length overflows"))?;
    if end > buffer_len {
        return Err(invalid_columns(
            path,
            &format!("block range [{offset}, {end}) exceeds file length {buffer_len}"),
        ));
    }
    Ok((offset, body_len, meta_len))
}

fn invalid_columns(path: &Path, detail: &str) -> StoreError {
    StoreError::InvalidColumns {
        path: path.to_path_buf(),
        detail: detail.to_string(),
    }
}

/// The row range `tile` occupies within `seg`'s Morton order, found by binary search over
/// `seg.morton.u64()` (contracts §2.5). Callers must treat a tile as resolving to a **set** of
/// ranges — one per segment sharing the tile's slice — even though Phase 1 has exactly one
/// segment per slice; the engine-level signature is `Vec<Range<u32>>` (task brief).
pub fn tile_ranges(seg: &SegmentData, tile: &Tile) -> Range<u32> {
    let codes = seg.morton.u64();
    let (lo, hi) = tile.code_range();
    let start = codes.partition_point(|&c| c < lo);
    let end = codes.partition_point(|&c| c < hi);
    start as u32..end as u32
}
