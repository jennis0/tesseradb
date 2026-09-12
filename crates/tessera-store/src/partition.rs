//! The partition: the primitive a pass that produces values in one order and needs them in
//! another goes through, with the receipts and anchors that make a spill file's bytes trustworthy.
//!
//! **It lives here rather than in the build because the build and the engine's fold run the same
//! pass** (owner ruling, 2026-09-12;
//! `docs/evidence/memos/2026-09-12-bounded-assembly-design.md` §4.6). The row column is composed by
//! one implementation on both sides, so the disk-backed primitive that composition goes through has
//! to be visible to both — and this crate is the one they share. `tessera-build` re-imports every
//! item below under `crate::spill`, so its own call sites read as they always did; what it keeps
//! for itself is the Morton routing and `boundaries_from_histogram`, which only a build has a key
//! uneven enough to need.
//!
//! Everything here is **fail-closed**: `finish` returns a [`SpillReceipt`] carrying the record
//! count and a content anchor (a wrapping sum of [`mix64`] over each record), and every read path
//! verifies both before its contents are trusted. A truncated, tampered or doubly-appended spill
//! file surfaces as a typed error, never as a silent partial read — on the build's side these files
//! feed the permanent entity-ID assignment (I9), so an undetected short read would be baked into
//! every bundle the deployment ever ships.
//!
//! Mismatches are reported as [`StoreError::MalformedBundle`] naming the file and the mismatch
//! kind; the message carries the discrimination a caller or operator needs.

use std::fs::{self, File};
use std::io::{BufWriter, Write};
use std::path::{Path, PathBuf};

use crate::error::{Result, StoreError};

/// A spill file's bytes are not the bytes that were written — the one shape of failure this
/// module reports, with the file and the mismatch named.
fn torn(detail: String) -> StoreError {
    StoreError::MalformedBundle { detail }
}

/// An I/O failure on a spill file, with the path that failed.
fn io(path: &Path, source: std::io::Error) -> StoreError {
    StoreError::Io {
        path: path.to_path_buf(),
        source,
    }
}

/// Buffer size for spill I/O, both directions. Four mebibytes: large enough that the syscall
/// cost is noise against the encode/decode work, small enough to be irrelevant against the
/// build's peak memory.
pub const SPILL_BUF_BYTES: usize = 4 << 20;

/// splitmix64's finalizer — a private **twin of `tessera_build`'s identity mixer**, same constants (the ones
/// contracts §2.6 fixes for the identity construction).
///
/// Duplicated rather than shared or passed in: the pipeline's copy is private to a file this
/// task may not modify, and threading a fn pointer through every writer just to avoid a
/// three-line pure function would couple this module's API to its first caller. The constants
/// are pinned by [`tests::mix64_matches_the_splitmix64_test_vector`], so the twins cannot
/// drift silently.
///
/// Why a mixed sum and not a plain one: a plain sum of raw values can be *compensated* —
/// replace records `{1, 3}` with `{2, 2}` and count and sum both survive — so each record is
/// put through a full-avalanche mixer first. Not cryptographic, and not meant to be: this
/// defends against torn writes, truncation and accidental double-appends, not an adversary
/// with write access to the build's own scratch directory.
pub fn mix64(mut x: u64) -> u64 {
    x = x.wrapping_add(0x9E37_79B9_7F4A_7C15);
    x = (x ^ (x >> 30)).wrapping_mul(0xBF58_476D_1CE4_E5B9);
    x = (x ^ (x >> 27)).wrapping_mul(0x94D0_49BB_1331_11EB);
    x ^ (x >> 31)
}



/// What `finish` hands back and every read path verifies against: the file, how many records
/// it holds, and the content anchor over those records.
///
/// The receipt lives in the build's memory, never on disk — a receipt stored beside the file
/// it vouches for could be tampered with in the same incident, and the build that wrote the
/// spill is the only reader it will ever have.
#[derive(Debug, Clone)]
pub struct SpillReceipt {
    pub path: PathBuf,
    /// Records written: `u64` values for a bucket file, `(term, entity)` pairs for a band file.
    pub count: u64,
    /// Wrapping sum of [`mix64`] over each record — the raw `u64` for a bucket file,
    /// [`pack_pair`] for a band file.
    pub anchor: u64,
}

// --------------------------------------------------------------------------------------------
// Bucket files
// --------------------------------------------------------------------------------------------

/// Appends one bucket file: packed `u64` values (`ordinal << 32 | term`).
///
/// **On disk:** each value as 8 bytes little-endian, concatenated; no header, no trailer, no
/// padding — `count * 8` bytes exactly. Integrity is external, via the [`SpillReceipt`].
pub struct SpillWriter {
    path: PathBuf,
    writer: BufWriter<File>,
    count: u64,
    anchor: u64,
}

impl SpillWriter {
    pub fn create(path: &Path) -> Result<SpillWriter> {
        let file = File::create(path).map_err(|e| io(path, e))?;
        Ok(SpillWriter {
            path: path.to_path_buf(),
            writer: BufWriter::with_capacity(SPILL_BUF_BYTES, file),
            count: 0,
            anchor: 0,
        })
    }

    /// A [`Partition`]'s bucket writer: the same file at whatever buffer the partition sized for
    /// it ([`partition_buffer_bytes`]), there being 128 or more of them open at once.
    pub fn create_sized(path: &Path, buffer: usize) -> Result<SpillWriter> {
        let file = File::create(path).map_err(|e| io(path, e))?;
        Ok(SpillWriter {
            path: path.to_path_buf(),
            writer: BufWriter::with_capacity(buffer, file),
            count: 0,
            anchor: 0,
        })
    }

    pub fn push(&mut self, value: u64) -> Result<()> {
        self.writer
            .write_all(&value.to_le_bytes())
            .map_err(|e| io(&self.path, e))?;
        self.count += 1;
        self.anchor = self.anchor.wrapping_add(mix64(value));
        Ok(())
    }

    /// One fixed-width record, for a [`Partition`]'s bucket. The anchor is
    /// [`mix64_bytes`]'s, so the record's width is part of what the receipt vouches for.
    pub fn push_bytes(&mut self, record: &[u8]) -> Result<()> {
        self.writer
            .write_all(record)
            .map_err(|e| io(&self.path, e))?;
        self.count += 1;
        self.anchor = self.anchor.wrapping_add(mix64_bytes(record));
        Ok(())
    }

    /// Flush, fsync, and hand back the receipt the eventual [`read_bucket`] must be given.
    pub fn finish(self) -> Result<SpillReceipt> {
        let SpillWriter {
            path,
            writer,
            count,
            anchor,
        } = self;
        let file = writer
            .into_inner()
            .map_err(|e| io(&path, e.into_error()))?;
        file.sync_all().map_err(|e| io(&path, e))?;
        Ok(SpillReceipt {
            path,
            count,
            anchor,
        })
    }
}

/// Read a whole bucket file back, verifying byte length *and* content anchor against the
/// receipt before returning a single value. Loading whole is by design — the caller's
/// pre-flight arithmetic sized the batch so its buckets fit in RAM.
pub fn read_bucket(receipt: &SpillReceipt) -> Result<Vec<u64>> {
    // Checked: `count` comes from our own receipt, but a length computation that can wrap is a
    // length computation that can be made to lie, so the multiply is guarded regardless of
    // provenance. (No `count <= u32::MAX` cap here — callers enforce their own.)
    let expected_bytes = receipt.count.checked_mul(8).ok_or_else(|| {
        torn(format!(
            "bucket file {}: receipt count {} overflows the byte-length computation",
            receipt.path.display(),
            receipt.count
        ))
    })?;
    let bytes = fs::read(&receipt.path).map_err(|e| io(&receipt.path, e))?;
    if bytes.len() as u64 != expected_bytes {
        return Err(torn(format!(
            "bucket file {}: length mismatch: file is {} bytes but the receipt's count {} \
             requires exactly {expected_bytes}",
            receipt.path.display(),
            bytes.len(),
            receipt.count
        )));
    }
    let mut values = Vec::with_capacity(bytes.len() / 8);
    let mut anchor = 0u64;
    for chunk in bytes.chunks_exact(8) {
        let value = u64::from_le_bytes(chunk.try_into().expect("chunks_exact(8) yields 8 bytes"));
        anchor = anchor.wrapping_add(mix64(value));
        values.push(value);
    }
    if anchor != receipt.anchor {
        return Err(torn(format!(
            "bucket file {}: content anchor mismatch: recomputed {anchor:#018x} but the \
             receipt says {:#018x} — the file's bytes are not the bytes that were written",
            receipt.path.display(),
            receipt.anchor
        )));
    }
    Ok(values)
}

// --------------------------------------------------------------------------------------------
// Partitions
// --------------------------------------------------------------------------------------------

/// How many buckets a [`Partition`] has. **Always 128.**
///
/// Every key a partition routes on is a `u32`, so a bucket of a uniform key holds at most
/// 2³² / 128 = 33.6×10⁶ records — 537 MB at 16 bytes a record, and a window over them at most
/// 33.6×10⁶ times the value's width. The partition's memory is then bounded by the key type and
/// not by the corpus or by the budget: one bucket, one window, and
/// [`PARTITION_BUF_BYTES`] × 128 of writer buffers. The pre-flight charges that as a constant.
///
/// **Fixed rather than derived from the budget**, which a stride could have been: a derived count
/// is one more term the residency model can get wrong, and the `u32` ceiling makes the fixed one
/// cheap enough that there is nothing to buy by deriving it.
pub const PARTITION_BUCKETS: usize = 128;

/// The largest a bucket writer's buffer gets. A mebibyte, 128 MiB over a uniform partition — and
/// what a corpus large enough to fill them gets; see [`partition_buffer_bytes`].
pub const PARTITION_BUF_BYTES: usize = 1 << 20;

/// The smallest. A page, so a partition over a handful of rows costs a page a bucket and not a
/// mebibyte: at 4,000 rows the full-size buffers are 257 MiB of anonymous memory against 48 kB of
/// records, which is a pre-flight refusal on a fixture that fits in a cache line's worth of disk.
pub const PARTITION_MIN_BUF_BYTES: usize = 4096;

/// One bucket writer's buffer, for a partition expecting `records` records of `record_width` over
/// `buckets` buckets: what one bucket will hold, clamped to [`PARTITION_MIN_BUF_BYTES`] and
/// [`PARTITION_BUF_BYTES`].
///
/// **The residency model computes the same number from the same inputs**, so what the pre-flight
/// charges is what the pass allocates.
pub fn partition_buffer_bytes(records: u64, record_width: usize, buckets: u64) -> usize {
    let per_bucket = records
        .saturating_mul(record_width as u64)
        .div_ceil(buckets.max(1));
    (per_bucket as usize).clamp(PARTITION_MIN_BUF_BYTES, PARTITION_BUF_BYTES)
}

/// The most records one bucket can hold, **whatever the corpus**: every key a partition routes on
/// is a `u32`, so 2³² keys over [`PARTITION_BUCKETS`] buckets is 33.55×10⁶ records. This is the
/// constant the residency model charges a partition's loaded bucket and window at — the type's
/// bound, not the corpus's, which is what makes the whole primitive's memory a constant
/// (`docs/evidence/memos/2026-09-12-bounded-assembly-design.md` §3).
pub const PARTITION_BUCKET_RECORDS: u64 = (1u64 << 32) / PARTITION_BUCKETS as u64;

/// The buckets a partition over a **counted** key can come to, against the 128 a uniform one has:
/// [`boundaries_from_histogram`] closes a bucket only when the next key would pass the target, so a
/// closed bucket and the key that closed it exceed it together and consecutive buckets sum to more
/// than one target. The bound is `2 × rows / target + 1`, so it is 257 only where
/// `target ≥ rows / 128`. The caller's target is `tessera-build`’s `assembly::MortonHistogram::target`,
/// which rounds **up**: a target of `rows / 128` rounded down is one short on all but the exact
/// multiples, and at 200 rows a floored target of 1 admits 401 buckets against the 257 this claims.
pub const PARTITION_COUNTED_BUCKETS: u64 = 2 * PARTITION_BUCKETS as u64 + 1;

/// Boundaries for a key that is **dense and uniform** over `[0, key_bound)`: an entity index, a
/// row index, an ordinal.
///
/// The first key of each bucket, ascending, starting at 0. A `key_bound` below the bucket count
/// gives a partition with empty buckets above the keys, which costs a file each and keeps every
/// caller's bucket loop the same shape.
pub fn boundaries_uniform(key_bound: u64) -> Vec<u32> {
    let bound = key_bound.max(1);
    (0..PARTITION_BUCKETS as u64)
        .map(|k| (bound.saturating_mul(k) / PARTITION_BUCKETS as u64).min(u32::MAX as u64) as u32)
        // A boundary equal to its predecessor would make a bucket unreachable rather than empty,
        // which `route` must not be asked to break a tie in. Dedup keeps them strictly ascending
        // and the partition then has fewer than 128 buckets, which is what a key space smaller
        // than the bucket count has.
        .collect::<std::collections::BTreeSet<u32>>()
        .into_iter()
        .collect()
}

/// The bucket a key belongs to: the last boundary at or below it.
fn route(boundaries: &[u32], key: u32) -> usize {
    boundaries.partition_point(|&first| first <= key).max(1) - 1
}

/// **The primitive a pass that produces values in one order and needs them in another goes
/// through.**
///
/// The values are appended to key-range buckets on disk; each bucket is then read whole into a
/// window, put in order there, and written out sequentially. Memory is one bucket and one window;
/// disk is one copy of the values, released bucket by bucket.
///
/// **Why this rather than a mapped array written at a scattered index.** A mapped file written at
/// a scattered index is bounded in memory only while the page cache holds it, and the cache is
/// whatever the rest of the build leaves. The keyword dictionary's scatter read 16 TB in four
/// hours for 49% of one stage on a box whose cache had been taken by a dead heap, and the
/// attribute tail wrote 123 GB to grow the bundle by 34
/// (`docs/evidence/memos/2026-09-12-gbif-whole-corpus-build-observations.md`). A partition's cost
/// does not depend on what else the build holds.
///
/// **A record is fixed-width and its first four bytes are a little-endian `u32` key.** The key is
/// what the record is routed by, and carrying it in the record is what lets a bucket be read back
/// as bytes with no side table.
///
/// **An uneven key needs boundaries of its own.** [`boundaries_uniform`] is for a key that is
/// dense and uniform — an entity index, a row index, an ordinal. A key that is neither, a Morton
/// code being the case in hand, needs boundaries from a counting pass over it, which is the
/// caller's to make: only the caller knows what a finer key of its own space is when one bin of
/// its histogram is over the target on its own. Not built yet: no such caller exists, so
/// `create` takes whatever boundaries it is handed and checks only that they ascend from zero.
pub struct Partition {
    /// One writer per bucket, in bucket order.
    writers: Vec<SpillWriter>,
    /// The first key of each bucket, for a partition that routes its own records — empty for one
    /// whose caller routes them ([`Partition::create_routed`]).
    boundaries: Vec<u32>,
    record_width: usize,
}

/// What [`Partition::finish`] hands back: the buckets, each verifiable and readable once.
pub struct PartitionStore {
    receipts: Vec<Option<SpillReceipt>>,
    record_width: usize,
}

impl Partition {
    /// Create the partition's bucket files under `dir`, named `<name>-<bucket>.part`.
    ///
    /// `boundaries` must be ascending, distinct and start at 0 — which is what both boundary
    /// constructors here produce.
    pub fn create(
        dir: &Path,
        name: &str,
        boundaries: Vec<u32>,
        record_width: usize,
        records: u64,
    ) -> Result<Partition> {
        if record_width < 4 {
            return Err(torn(format!(
                "partition {name}: a record is {record_width} bytes and its first four are \
                 its key"
            )));
        }
        if boundaries.first() != Some(&0) || boundaries.windows(2).any(|w| w[0] >= w[1]) {
            return Err(torn(format!(
                "partition {name}: the boundaries must ascend from 0 and be distinct"
            )));
        }
        let buffer = partition_buffer_bytes(records, record_width, boundaries.len() as u64);
        let writers = (0..boundaries.len())
            .map(|k| {
                SpillWriter::create_sized(&dir.join(format!("{name}-{k:03}.part")), buffer)
            })
            .collect::<Result<_>>()?;
        Ok(Partition {
            writers,
            boundaries,
            record_width,
        })
    }

    /// Append one record, routed by the `u32` its first four bytes carry.
    pub fn push(&mut self, record: &[u8]) -> Result<()> {
        debug_assert_eq!(record.len(), self.record_width);
        let key = u32::from_le_bytes(record[..4].try_into().expect("a record is at least 4 bytes"));
        self.writers[route(&self.boundaries, key)].push_bytes(record)
    }

    /// A partition whose buckets the **caller** routes to, by whatever key of its own it holds.
    ///
    /// The row partition of the segment assembly is the one caller: its key is a Morton code
    /// refined, where one code carries more rows than a bucket should, by the `priority` prefix of
    /// the identity — a composite that does not fit the four bytes a record's key is. Its
    /// boundaries are the caller's and so is the binary search over them; what the partition still
    /// owns is the one-writer-per-bucket append, the receipts, and the read-once load.
    ///
    /// [`Partition::push`] and [`Partition::range`] are not available on one of these: there is no
    /// key range here for the partition to know.
    pub fn create_routed(
        dir: &Path,
        name: &str,
        buckets: usize,
        record_width: usize,
        records: u64,
    ) -> Result<Partition> {
        if buckets == 0 {
            return Err(torn(format!(
                "partition {name}: a routed partition has at least one bucket"
            )));
        }
        let buffer = partition_buffer_bytes(records, record_width, buckets as u64);
        let writers = (0..buckets)
            .map(|k| SpillWriter::create_sized(&dir.join(format!("{name}-{k:03}.part")), buffer))
            .collect::<Result<_>>()?;
        Ok(Partition {
            writers,
            boundaries: Vec::new(),
            record_width,
        })
    }

    /// Append one record to the bucket the caller routed it to — the counterpart of
    /// [`Partition::create_routed`].
    pub fn push_to(&mut self, bucket: usize, record: &[u8]) -> Result<()> {
        debug_assert_eq!(record.len(), self.record_width);
        self.writers[bucket].push_bytes(record)
    }

    /// How many buckets this partition has — the writers, not the boundaries: a routed partition
    /// ([`Partition::create_routed`]) has its buckets and none of the boundaries a self-routing one
    /// derives them from.
    pub fn buckets(&self) -> usize {
        self.writers.len()
    }

    /// The key range bucket `k` covers, `hi` exclusive — what a caller sizes its window by.
    pub fn range(&self, k: usize) -> (u32, u64) {
        let lo = self.boundaries[k];
        let hi = self
            .boundaries
            .get(k + 1)
            .map(|&first| first as u64)
            .unwrap_or(1u64 << 32);
        (lo, hi)
    }

    /// Flush and fsync every bucket, and hand back the store the reads go through.
    ///
    /// **The writers are taken out rather than moved out of `self`**: the partition owns a `Drop`
    /// that unlinks its files, so a destructuring move is not available and a `self` left holding
    /// them would sweep away the very buckets this just finished.
    pub fn finish(mut self) -> Result<PartitionStore> {
        let writers = std::mem::take(&mut self.writers);
        Ok(PartitionStore {
            receipts: writers
                .into_iter()
                .map(|w| w.finish().map(Some))
                .collect::<Result<_>>()?,
            record_width: self.record_width,
        })
    }
}

impl Drop for Partition {
    /// **The bucket files go back with the partition it failed part-way through.** A build that
    /// dies between [`Partition::create`] and [`Partition::finish`] would otherwise leave 128
    /// files behind; [`PartitionStore`] has the same sweep for the half of the life after
    /// `finish`. Best effort, as `MappedArray`'s is: `TmpDir` covers whatever this misses, and a
    /// partition whose files are not under it (see [`Partition::create`]'s callers) is swept by
    /// the build's own directory removal.
    fn drop(&mut self) {
        for writer in &self.writers {
            let _ = std::fs::remove_file(&writer.path);
        }
    }
}

impl PartitionStore {
    /// Bucket `k`'s records as raw bytes, read once and verified against the receipt.
    pub fn load(&self, k: usize) -> Result<Vec<u8>> {
        let receipt = self.receipts[k]
            .as_ref()
            .ok_or_else(|| {
                torn(format!(
                    "partition bucket {k} loaded after it was deleted — a bucket may be read as \
                     many times as a pass wants and released once"
                ))
            })?;
        read_bucket_bytes(receipt, self.record_width)
    }

    /// Release bucket `k`'s file — the disk comes back as the pass walks the buckets rather than
    /// at the end of it.
    pub fn delete(&mut self, k: usize) -> Result<()> {
        if let Some(receipt) = self.receipts[k].take() {
            std::fs::remove_file(&receipt.path).map_err(|e| io(&receipt.path, e))?;
        }
        Ok(())
    }
}

impl Drop for PartitionStore {
    /// The buckets go back with the store, whichever of them the pass did not reach. With
    /// [`Partition`]'s own sweep this covers the whole of a partition's life, so a build that
    /// fails anywhere in it leaves no bucket file behind.
    fn drop(&mut self) {
        for receipt in self.receipts.iter_mut().flatten() {
            let _ = std::fs::remove_file(&receipt.path);
        }
    }
}

/// [`read_bucket`] for a partition's fixed-width records: the bytes, verified by length and by
/// content anchor, with the record width checked against the file's length.
fn read_bucket_bytes(receipt: &SpillReceipt, record_width: usize) -> Result<Vec<u8>> {
    let expected_bytes = receipt
        .count
        .checked_mul(record_width as u64)
        .ok_or_else(|| {
            torn(format!(
                "partition bucket {}: receipt count {} overflows the byte-length computation",
                receipt.path.display(),
                receipt.count
            ))
        })?;
    let bytes = fs::read(&receipt.path).map_err(|e| io(&receipt.path, e))?;
    if bytes.len() as u64 != expected_bytes {
        return Err(torn(format!(
            "partition bucket {}: length mismatch: file is {} bytes but the receipt's count {} \
             requires exactly {expected_bytes}",
            receipt.path.display(),
            bytes.len(),
            receipt.count
        )));
    }
    let mut anchor = 0u64;
    for record in bytes.chunks_exact(record_width) {
        anchor = anchor.wrapping_add(mix64_bytes(record));
    }
    if anchor != receipt.anchor {
        return Err(torn(format!(
            "partition bucket {}: content anchor mismatch: recomputed {anchor:#018x} but the \
             receipt says {:#018x} — the file's bytes are not the bytes that were written",
            receipt.path.display(),
            receipt.anchor
        )));
    }
    Ok(bytes)
}

/// [`mix64`] over a record's bytes: each 8-byte group mixed and summed, the tail zero-padded.
///
/// A mixed sum rather than a plain one for [`mix64`]'s own reason, and per group rather than per
/// record so a record of any width goes through the same arithmetic.
fn mix64_bytes(record: &[u8]) -> u64 {
    let mut anchor = 0u64;
    for group in record.chunks(8) {
        let mut word = [0u8; 8];
        word[..group.len()].copy_from_slice(group);
        anchor = anchor.wrapping_add(mix64(u64::from_le_bytes(word)));
    }
    anchor
}

#[cfg(test)]
mod tests {
    use super::*;

    fn err_string(result: Result<impl std::fmt::Debug>) -> String {
        result.expect_err("expected an error").to_string()
    }

    fn write_bucket(path: &Path, values: &[u64]) -> SpillReceipt {
        let mut writer = SpillWriter::create(path).unwrap();
        for &value in values {
            writer.push(value).unwrap();
        }
        writer.finish().unwrap()
    }

    fn flip_byte(path: &Path, index: usize) {
        let mut bytes = fs::read(path).unwrap();
        bytes[index] ^= 0x40;
        fs::write(path, bytes).unwrap();
    }

    fn truncate_by(path: &Path, n: usize) {
        let bytes = fs::read(path).unwrap();
        fs::write(path, &bytes[..bytes.len() - n]).unwrap();
    }

    fn append(path: &Path, extra: &[u8]) {
        let mut bytes = fs::read(path).unwrap();
        bytes.extend_from_slice(extra);
        fs::write(path, bytes).unwrap();
    }

    /// Pins the constants against `tessera-build`'s `pipeline::mix64` (both are splitmix64's
    /// finalizer): the
    /// widely published first output of splitmix64 seeded with 0. If either twin's constants
    /// drift, one of the two crates' copies of this vector fails.
    #[test]
    fn mix64_matches_the_splitmix64_test_vector() {
        assert_eq!(mix64(0), 0xE220_A839_7B1D_CDAF);
    }


    // ---- bucket files -----------------------------------------------------------------

    #[test]
    fn bucket_round_trips() {
        let temp = tempfile::TempDir::new().unwrap();
        let cases: Vec<Vec<u64>> = vec![
            vec![],
            vec![0],
            vec![42],
            vec![u64::MAX],
            // The packed shape the pipeline writes: ordinal << 32 | term, at u32 boundaries.
            vec![
                0,
                1,
                u32::MAX as u64,
                (1u64 << 32) | 7,
                ((u32::MAX as u64) << 32) | u32::MAX as u64,
            ],
            (0..10_000).map(|i| i * 0x9E37).collect(),
        ];
        for (i, values) in cases.iter().enumerate() {
            let path = temp.path().join(format!("bucket-{i}.u64"));
            let receipt = write_bucket(&path, values);
            assert_eq!(receipt.count, values.len() as u64);
            assert_eq!(read_bucket(&receipt).unwrap(), *values);
        }
    }

    #[test]
    fn bucket_flip_is_an_anchor_mismatch() {
        let temp = tempfile::TempDir::new().unwrap();
        let path = temp.path().join("bucket.u64");
        let receipt = write_bucket(&path, &[1, 2, 3, 4]);
        flip_byte(&path, 9);
        let message = err_string(read_bucket(&receipt));
        assert!(message.contains("anchor mismatch"), "got: {message}");
        assert!(message.contains("bucket.u64"), "got: {message}");
    }

    #[test]
    fn bucket_truncation_is_a_length_mismatch() {
        let temp = tempfile::TempDir::new().unwrap();
        let path = temp.path().join("bucket.u64");
        let receipt = write_bucket(&path, &[1, 2, 3]);
        for cut in [1usize, 8] {
            let receipt = receipt.clone();
            write_bucket(&path, &[1, 2, 3]);
            truncate_by(&path, cut);
            let message = err_string(read_bucket(&receipt));
            assert!(message.contains("length mismatch"), "got: {message}");
        }
    }

    #[test]
    fn bucket_trailing_garbage_is_a_length_mismatch() {
        let temp = tempfile::TempDir::new().unwrap();
        let path = temp.path().join("bucket.u64");
        let receipt = write_bucket(&path, &[5, 6]);
        // A whole extra value as well as a ragged byte: both must fail on length.
        for extra in [&[0u8; 8][..], &[0xAB][..]] {
            write_bucket(&path, &[5, 6]);
            append(&path, extra);
            let message = err_string(read_bucket(&receipt));
            assert!(message.contains("length mismatch"), "got: {message}");
        }
    }

    #[test]
    fn bucket_length_multiply_cannot_overflow() {
        let temp = tempfile::TempDir::new().unwrap();
        let path = temp.path().join("bucket.u64");
        write_bucket(&path, &[]);
        let receipt = SpillReceipt {
            path,
            count: u64::MAX / 2,
            anchor: 0,
        };
        let message = err_string(read_bucket(&receipt));
        assert!(message.contains("overflows"), "got: {message}");
    }

    // ---- band files -------------------------------------------------------------------


    // ----------------------------------------------------------------------------------------
    // Partitions
    // ----------------------------------------------------------------------------------------

    fn record(key: u32, payload: u32) -> [u8; 8] {
        let mut out = [0u8; 8];
        out[..4].copy_from_slice(&key.to_le_bytes());
        out[4..].copy_from_slice(&payload.to_le_bytes());
        out
    }

    /// The headline: every record comes back, in its own bucket, in the order it was pushed.
    #[test]
    fn a_partition_returns_every_record_in_its_bucket_in_push_order() {
        let dir = tempfile::tempdir().expect("tempdir");
        let boundaries = boundaries_uniform(1_000);
        let mut part =
            Partition::create(dir.path(), "rows", boundaries.clone(), 8, 64).expect("create");
        // Pushed in an order that is neither the key's nor a bucket's.
        for i in 0..1_000u32 {
            let key = (i * 617) % 1_000;
            part.push(&record(key, i)).expect("push");
        }
        let mut store = part.finish().expect("finish");
        let mut seen = vec![None; 1_000];
        for k in 0..boundaries.len() {
            let (lo, hi) = (
                boundaries[k] as u64,
                boundaries.get(k + 1).map(|&b| b as u64).unwrap_or(1 << 32),
            );
            let bytes = store.load(k).expect("load");
            let mut previous: Option<u32> = None;
            for chunk in bytes.chunks_exact(8) {
                let key = u32::from_le_bytes(chunk[..4].try_into().unwrap());
                let payload = u32::from_le_bytes(chunk[4..].try_into().unwrap());
                assert!(
                    (key as u64) >= lo && (key as u64) < hi,
                    "bucket {k} holds key {key} outside [{lo}, {hi})"
                );
                if let Some(before) = previous {
                    assert!(
                        payload > before,
                        "a bucket is append order, and {payload} followed {before}"
                    );
                }
                previous = Some(payload);
                assert!(seen[key as usize].is_none(), "key {key} appeared twice");
                seen[key as usize] = Some(payload);
            }
            store.delete(k).expect("delete");
        }
        assert!(seen.iter().all(Option::is_some), "every key came back");
    }

    /// A bucket file whose bytes are not the bytes that were written is refused, not decoded.
    #[test]
    fn a_flipped_byte_in_a_bucket_is_an_anchor_mismatch() {
        let dir = tempfile::tempdir().expect("tempdir");
        let mut part =
            Partition::create(dir.path(), "rows", boundaries_uniform(8), 8, 64).expect("create");
        for key in 0..8u32 {
            part.push(&record(key, key)).expect("push");
        }
        let store = part.finish().expect("finish");
        let path = store.receipts[0].as_ref().expect("bucket 0").path.clone();
        let mut bytes = fs::read(&path).expect("read");
        bytes[4] ^= 1;
        fs::write(&path, &bytes).expect("write");
        let error = store.load(0).expect_err("a flipped byte is refused");
        assert!(
            format!("{error}").contains("content anchor mismatch"),
            "{error}"
        );
    }

    /// A truncated bucket is refused on its length before a record is decoded.
    #[test]
    fn a_truncated_bucket_is_a_length_mismatch() {
        let dir = tempfile::tempdir().expect("tempdir");
        let mut part =
            Partition::create(dir.path(), "rows", boundaries_uniform(8), 8, 64).expect("create");
        for key in 0..8u32 {
            part.push(&record(key, key)).expect("push");
        }
        let store = part.finish().expect("finish");
        let path = store.receipts[0].as_ref().expect("bucket 0").path.clone();
        let bytes = fs::read(&path).expect("read");
        fs::write(&path, &bytes[..bytes.len() - 8]).expect("write");
        let error = store.load(0).expect_err("a short file is refused");
        assert!(format!("{error}").contains("length mismatch"), "{error}");
    }

    /// The uniform boundaries ascend from zero, are distinct, and give 128 buckets wherever the
    /// key space has room for them.
    #[test]
    fn uniform_boundaries_ascend_from_zero_and_are_distinct() {
        let wide = boundaries_uniform(1u64 << 32);
        assert_eq!(wide.len(), PARTITION_BUCKETS);
        assert_eq!(wide[0], 0);
        assert!(wide.windows(2).all(|w| w[0] < w[1]));
        assert_eq!(wide[1], 1u32 << (32 - 7));
        // A key space smaller than the bucket count gives one bucket a key, not an unreachable
        // bucket.
        let narrow = boundaries_uniform(5);
        assert_eq!(narrow, vec![0, 1, 2, 3, 4]);
        assert_eq!(boundaries_uniform(0), vec![0]);
    }

    /// A record narrower than its key, and boundaries that do not ascend, are refused at creation.
    #[test]
    fn a_partition_refuses_a_record_without_room_for_a_key_and_boundaries_that_repeat() {
        let dir = tempfile::tempdir().expect("tempdir");
        assert!(Partition::create(dir.path(), "rows", vec![0], 3, 1 << 20).is_err());
        assert!(Partition::create(dir.path(), "rows", vec![1, 2], 8, 1 << 20).is_err());
        assert!(Partition::create(dir.path(), "rows", vec![0, 2, 2], 8, 1 << 20).is_err());
    }}
