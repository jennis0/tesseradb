//! Zero-copy, mmap-backed reader for one `external-ids-<n>.arrow` extent (contracts §2.1,
//! Reference Sheet R4): a two-column `(external_id: Binary, entity_id: UInt64)` record batch,
//! sorted ascending by `external_id`'s bytes within the extent (build writes the global sort
//! split sequentially into extents, so extent *k*'s ids all precede extent *k+1*'s).
//!
//! At 10⁹ rows the old engine-side loader (`Vec<Vec<u8>>` of every id, plus a parallel entity
//! vec, concatenated and re-sorted across extents) is ~48 GB — one heap allocation per id and a
//! full sort, entirely avoidable because each extent already arrives sorted. This reader instead
//! reuses [`crate::read::decode_single_batch`]'s zero-copy IPC decode (mmap → `arrow::buffer::
//! Buffer` → `FileDecoder`, alignment-checked, uncompressed-only — the same technique
//! `ColumnsRef` and `tessera_authz::postings::PostingsReader` use) and binary-searches directly
//! over the mapped `BinaryArray`: O(1) heap allocations and O(row bytes) resident memory per
//! extent, no per-id copy, no re-sort.

use std::fs::File;
use std::path::{Path, PathBuf};
use std::ptr::NonNull;
use std::sync::Arc;

use arrow::array::{Array, BinaryArray, UInt64Array};
use arrow::buffer::Buffer;
use arrow::datatypes::DataType;
use arrow::record_batch::RecordBatch;
use memmap2::Mmap;

use crate::error::{Result, StoreError};
use crate::read::decode_single_batch;

/// One loaded, digest-verified `external-ids-<n>.arrow` extent: a zero-copy view over its two
/// columns, plus the entity ids as a typed slice (both are validated to be non-null, fixed-width
/// or offset-indexed arrays with no null buffer at [`ExternalIdExtent::load`]).
#[derive(Debug)]
pub struct ExternalIdExtent {
    batch: RecordBatch,
}

impl ExternalIdExtent {
    /// Load and validate `path`. Callers are responsible for having already verified the file's
    /// digest via the bundle read protocol (`open_bundle`'s `verify_files` covers every path in
    /// `MANIFEST.json`'s `files` map, which includes every `external_id_extents` entry) — this
    /// loader does not re-verify, only re-reads the same file.
    pub fn load(path: &Path) -> Result<Self> {
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
        let len = mapping.len();
        let arc: Arc<Mmap> = Arc::new(mapping);
        let ptr = NonNull::new(arc.as_ptr() as *mut u8)
            .expect("memmap2::Mmap never returns a null base pointer");
        let buffer = unsafe { Buffer::from_custom_allocation(ptr, len, arc) };

        let batch = decode_single_batch(&buffer, path)?;
        validate_schema(&batch, path)?;
        validate_sorted(&batch, path)?;

        Ok(ExternalIdExtent { batch })
    }

    pub fn len(&self) -> usize {
        self.batch.num_rows()
    }

    pub fn is_empty(&self) -> bool {
        self.batch.num_rows() == 0
    }

    fn ext_col(&self) -> &BinaryArray {
        self.batch
            .column(0)
            .as_any()
            .downcast_ref::<BinaryArray>()
            .expect("validated at ExternalIdExtent::load")
    }

    fn ent_col(&self) -> &UInt64Array {
        self.batch
            .column(1)
            .as_any()
            .downcast_ref::<UInt64Array>()
            .expect("validated at ExternalIdExtent::load")
    }

    /// The external-id bytes at row `idx` (zero-copy, borrowed from the mapped buffer).
    pub fn key(&self, idx: usize) -> &[u8] {
        self.ext_col().value(idx)
    }

    /// The entity id at row `idx`.
    pub fn entity(&self, idx: usize) -> u64 {
        self.ent_col().value(idx)
    }

    /// This extent's first key, or `None` if it is empty.
    pub fn first_key(&self) -> Option<&[u8]> {
        (!self.is_empty()).then(|| self.key(0))
    }

    /// This extent's last key, or `None` if it is empty.
    pub fn last_key(&self) -> Option<&[u8]> {
        (!self.is_empty()).then(|| self.key(self.len() - 1))
    }

    /// Binary-search this extent for `external_id`, returning its entity id if present.
    pub fn resolve(&self, external_id: &[u8]) -> Option<u64> {
        let ext_col = self.ext_col();
        let idx = binary_search_by(self.len(), |i| ext_col.value(i).cmp(external_id)).ok()?;
        Some(self.entity(idx))
    }
}

/// `[0, n)` binary search parameterised on a comparator, so callers don't need a materialised
/// slice — `ext_col.value(i)` returns a borrowed `&[u8]` per call rather than requiring one
/// contiguous `&[&[u8]]` (which `BinaryArray`, being a single values buffer plus offsets, does
/// not have).
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

fn invalid(path: &Path, detail: impl Into<String>) -> StoreError {
    StoreError::InvalidExternalIds {
        path: path.to_path_buf(),
        detail: detail.into(),
    }
}

fn validate_schema(batch: &RecordBatch, path: &Path) -> Result<()> {
    let schema = batch.schema_ref();
    if schema.fields().len() != 2 {
        return Err(invalid(
            path,
            format!(
                "expected exactly 2 columns (external_id, entity_id), found {}",
                schema.fields().len()
            ),
        ));
    }
    let ext_field = schema.field(0);
    if ext_field.data_type() != &DataType::Binary || ext_field.is_nullable() {
        return Err(invalid(
            path,
            format!(
                "column 0 must be non-nullable Binary, found {:?} (nullable: {})",
                ext_field.data_type(),
                ext_field.is_nullable()
            ),
        ));
    }
    let ent_field = schema.field(1);
    if ent_field.data_type() != &DataType::UInt64 || ent_field.is_nullable() {
        return Err(invalid(
            path,
            format!(
                "column 1 must be non-nullable UInt64, found {:?} (nullable: {})",
                ent_field.data_type(),
                ent_field.is_nullable()
            ),
        ));
    }
    if batch.column(0).null_count() != 0 || batch.column(1).null_count() != 0 {
        return Err(invalid(path, "columns must have no nulls"));
    }
    Ok(())
}

/// R4 guarantees each extent is individually sorted ascending by external-id bytes — validate it
/// once at load, so `resolve`'s binary search can trust the invariant rather than silently
/// returning wrong (or missing) answers over an out-of-order extent. This is authorisation-bearing
/// (a mis-resolved external id in `/control/changes` denies the wrong entity and leaves the
/// intended target visible), so it fails closed rather than skip the check.
fn validate_sorted(batch: &RecordBatch, path: &Path) -> Result<()> {
    let ext_col = batch
        .column(0)
        .as_any()
        .downcast_ref::<BinaryArray>()
        .expect("schema validated");
    for i in 1..ext_col.len() {
        if ext_col.value(i - 1) >= ext_col.value(i) {
            return Err(invalid(
                path,
                format!(
                    "external_id column is not strictly ascending at row {i} \
                     (row {} >= row {i})",
                    i - 1
                ),
            ));
        }
    }
    Ok(())
}

/// The set of `external-ids-<n>.arrow` extents for one partition, addressed by extent-local
/// first/last key so a lookup does at most one extent's binary search plus an O(extents) scan to
/// pick which one — no data beyond each extent's own mapped bytes is held resident twice.
#[derive(Debug)]
pub struct ExternalIdIndex {
    extents: Vec<ExternalIdExtent>,
}

impl ExternalIdIndex {
    /// Load every extent in `paths`, in order (extent 0 first). Each `path` must already have
    /// been digest-verified by the bundle read protocol before this is called.
    pub fn load(paths: &[PathBuf]) -> Result<Self> {
        let extents = paths
            .iter()
            .map(|p| ExternalIdExtent::load(p))
            .collect::<Result<Vec<_>>>()?;
        Ok(ExternalIdIndex { extents })
    }

    /// Resolve `external_id` to its entity id, or `None` if it names nothing in this index.
    /// Fail-closed callers (I10/`/control/changes`) treat `None` as "not found in the bundle",
    /// distinct from any live-established mapping they may also consult.
    pub fn resolve(&self, external_id: &[u8]) -> Option<u64> {
        // Extents partition the global sorted order into consecutive ranges (build splits
        // sequentially from one globally sorted array), so the extent whose `last_key` is the
        // first to be `>= external_id` is the only one that can contain it.
        let extent_idx = self
            .extents
            .iter()
            .position(|e| e.last_key().is_none_or(|last| last >= external_id))?;
        self.extents[extent_idx].resolve(external_id)
    }
}

#[cfg(test)]
mod tests {
    use std::fs;

    use arrow::array::{ArrayRef, BinaryArray, UInt64Array};
    use arrow::datatypes::{DataType, Field, Schema};
    use arrow::ipc::writer::FileWriter;
    use arrow::record_batch::RecordBatch;
    use rand::rngs::StdRng;
    use rand::seq::SliceRandom;
    use rand::{Rng, SeedableRng};

    use super::*;

    /// Write one `external-ids-<n>.arrow`-shaped extent from already-sorted `rows` — the same
    /// on-disk shape `tessera_build::write_external_id_extent` produces, reimplemented here so
    /// this crate's tests don't need a `tessera-build` dev-dependency for what is otherwise a
    /// six-line Arrow IPC write.
    fn write_extent(path: &std::path::Path, rows: &[(Vec<u8>, u64)]) {
        let schema = Arc::new(Schema::new(vec![
            Field::new("external_id", DataType::Binary, false),
            Field::new("entity_id", DataType::UInt64, false),
        ]));
        let ext: ArrayRef = Arc::new(BinaryArray::from_iter_values(
            rows.iter().map(|(id, _)| id.as_slice()),
        ));
        let ent: ArrayRef = Arc::new(UInt64Array::from_iter_values(rows.iter().map(|(_, e)| *e)));
        let batch = RecordBatch::try_new(schema.clone(), vec![ext, ent]).unwrap();
        let file = File::create(path).unwrap();
        let mut writer = FileWriter::try_new(file, &schema).unwrap();
        writer.write(&batch).unwrap();
        writer.finish().unwrap();
    }

    /// The pre-fix loader (Task 10/13's original `ExternalIdIndex`): read every extent fully
    /// into owned `Vec<Vec<u8>>` + `Vec<u64>`, then re-sort the concatenation and binary-search
    /// linearly. Kept only as a test oracle — this is the ~48 GB-at-10⁹-rows behaviour the mmap
    /// reader must match exactly, not a code path anything production calls any more.
    struct OracleIndex {
        ids: Vec<Vec<u8>>,
        entities: Vec<u64>,
    }

    impl OracleIndex {
        fn load(paths: &[std::path::PathBuf]) -> Self {
            let mut ids: Vec<Vec<u8>> = Vec::new();
            let mut entities: Vec<u64> = Vec::new();
            for path in paths {
                let file = File::open(path).unwrap();
                let reader = arrow::ipc::reader::FileReader::try_new(file, None).unwrap();
                for batch in reader {
                    let batch = batch.unwrap();
                    let ext_col = batch
                        .column(0)
                        .as_any()
                        .downcast_ref::<BinaryArray>()
                        .unwrap();
                    let ent_col = batch
                        .column(1)
                        .as_any()
                        .downcast_ref::<UInt64Array>()
                        .unwrap();
                    for i in 0..batch.num_rows() {
                        ids.push(ext_col.value(i).to_vec());
                        entities.push(ent_col.value(i));
                    }
                }
            }
            let mut order: Vec<usize> = (0..ids.len()).collect();
            order.sort_by(|&a, &b| ids[a].cmp(&ids[b]));
            OracleIndex {
                ids: order.iter().map(|&i| ids[i].clone()).collect(),
                entities: order.iter().map(|&i| entities[i]).collect(),
            }
        }

        fn resolve(&self, external_id: &[u8]) -> Option<u64> {
            self.ids
                .binary_search_by(|probe| probe.as_slice().cmp(external_id))
                .ok()
                .map(|idx| self.entities[idx])
        }
    }

    /// Build a multi-extent fixture: `total_rows` distinct 8-byte keys, globally sorted then
    /// split sequentially into extents of `rows_per_extent` (mirroring
    /// `tessera_build::write_external_id_extents`'s splitting rule — extent *k*'s ids all
    /// precede extent *k+1*'s), written under `dir`.
    fn build_fixture(
        dir: &std::path::Path,
        total_rows: u64,
        rows_per_extent: usize,
    ) -> Vec<std::path::PathBuf> {
        let mut rows: Vec<(Vec<u8>, u64)> = (0..total_rows)
            .map(|i| {
                // Keys chosen so byte order and numeric order disagree (mirrors R4's
                // byte-swapped-source-id encoding), like the build's own fixture test.
                let key = (i.wrapping_mul(0x9E37_79B9) ^ 0xFFFF_FFFF_0000_0000).to_le_bytes();
                (key.to_vec(), i)
            })
            .collect();
        rows.sort_by(|a, b| a.0.cmp(&b.0));

        let mut paths = Vec::new();
        for (idx, chunk) in rows.chunks(rows_per_extent.max(1)).enumerate() {
            let path = dir.join(format!("external-ids-{idx}.arrow"));
            write_extent(&path, chunk);
            paths.push(path);
        }
        if paths.is_empty() {
            let path = dir.join("external-ids-0.arrow");
            write_extent(&path, &[]);
            paths.push(path);
        }
        paths
    }

    #[test]
    fn single_extent_resolves_present_and_absent_ids() {
        let temp = tempfile::TempDir::new().unwrap();
        let paths = build_fixture(temp.path(), 500, 10_000);
        assert_eq!(paths.len(), 1);

        let index = ExternalIdIndex::load(&paths).unwrap();
        let oracle = OracleIndex::load(&paths);

        for i in 0..500u64 {
            let key = (i.wrapping_mul(0x9E37_79B9) ^ 0xFFFF_FFFF_0000_0000).to_le_bytes();
            assert_eq!(index.resolve(&key), oracle.resolve(&key));
            assert_eq!(index.resolve(&key), Some(i));
        }
        assert_eq!(index.resolve(&[0xAB; 8]), None);
    }

    #[test]
    fn multi_extent_fixture_matches_the_oracle_across_extent_boundaries() {
        let temp = tempfile::TempDir::new().unwrap();
        // Small rows_per_extent so a few thousand rows produce several extents, exercising the
        // extent-selection step, not just within-extent binary search.
        for rows_per_extent in [1usize, 2, 3, 7, 64] {
            let dir = temp.path().join(format!("rpe-{rows_per_extent}"));
            fs::create_dir_all(&dir).unwrap();
            let total = 400u64;
            let paths = build_fixture(&dir, total, rows_per_extent);
            assert_eq!(paths.len(), (total as usize).div_ceil(rows_per_extent));

            let index = ExternalIdIndex::load(&paths).unwrap();
            let oracle = OracleIndex::load(&paths);

            for i in 0..total {
                let key = (i.wrapping_mul(0x9E37_79B9) ^ 0xFFFF_FFFF_0000_0000).to_le_bytes();
                assert_eq!(
                    index.resolve(&key),
                    oracle.resolve(&key),
                    "mismatch at rows_per_extent={rows_per_extent}, row {i}"
                );
                assert_eq!(index.resolve(&key), Some(i));
            }
        }
    }

    #[test]
    fn randomised_present_and_absent_probes_match_the_oracle() {
        let temp = tempfile::TempDir::new().unwrap();
        let dir = temp.path().join("random");
        fs::create_dir_all(&dir).unwrap();
        let total = 2_000u64;
        let paths = build_fixture(&dir, total, 137);

        let index = ExternalIdIndex::load(&paths).unwrap();
        let oracle = OracleIndex::load(&paths);

        let mut rng = StdRng::seed_from_u64(0xC0FF_EE42);
        let mut present_keys: Vec<[u8; 8]> = (0..total)
            .map(|i| (i.wrapping_mul(0x9E37_79B9) ^ 0xFFFF_FFFF_0000_0000).to_le_bytes())
            .collect();
        present_keys.shuffle(&mut rng);

        for _ in 0..1_000 {
            let key: [u8; 8] = if rng.gen_bool(0.5) {
                *present_keys.choose(&mut rng).unwrap()
            } else {
                rng.gen::<[u8; 8]>()
            };
            assert_eq!(
                index.resolve(&key),
                oracle.resolve(&key),
                "mismatch for probe key {key:?}"
            );
        }
    }

    #[test]
    fn empty_extent_resolves_nothing() {
        let temp = tempfile::TempDir::new().unwrap();
        let paths = build_fixture(temp.path(), 0, 4);
        assert_eq!(paths.len(), 1);
        let index = ExternalIdIndex::load(&paths).unwrap();
        assert_eq!(index.resolve(&[1, 2, 3, 4, 5, 6, 7, 8]), None);
    }

    #[test]
    fn rejects_an_out_of_order_extent() {
        let temp = tempfile::TempDir::new().unwrap();
        let path = temp.path().join("external-ids-0.arrow");
        // Deliberately out of ascending order.
        write_extent(
            &path,
            &[
                (vec![2, 0, 0, 0, 0, 0, 0, 0], 0),
                (vec![1, 0, 0, 0, 0, 0, 0, 0], 1),
            ],
        );
        let err = ExternalIdIndex::load(&[path]).unwrap_err();
        assert!(matches!(err, StoreError::InvalidExternalIds { .. }));
    }
}
