//! Spill files — the build's transient on-disk state under `<out>/.build-tmp/`.
//!
//! The batch-scoped signature-assignment rework spills two kinds of intermediate file during a
//! build and reads them back later in the same run:
//!
//! * **Bucket files** ([`SpillWriter`] / [`read_bucket`]): per-batch arrays of packed
//!   `ordinal << 32 | term` values, appended during one pairs scan and later loaded whole into
//!   RAM. The *load* is bounded by the caller's pre-flight arithmetic; this module's job is
//!   only to guarantee that the bytes read back are exactly the bytes written.
//! * **Band files** ([`BandWriter`] / [`BandReader`]): `(term, entity)` pair streams for a
//!   contiguous term range, appended across batches and decoded sequentially exactly once.
//!
//! Both are **fail-closed**: `finish` returns a [`SpillReceipt`] carrying the record count and
//! a content anchor (a wrapping sum of [`mix64`] over each record), and every read path
//! verifies both before its contents are trusted. A truncated, tampered or doubly-appended
//! spill file surfaces as a typed error, never as a silent partial read — these files feed the
//! permanent entity-ID assignment (I9), so an undetected short read here would be baked into
//! every bundle the deployment ever ships.
//!
//! Mismatches are reported as [`BuildError::Invalid`] naming the file and the mismatch kind:
//! this task adds only this module, so no new `BuildError` variant is introduced; the message
//! carries the discrimination a caller or operator needs.
//!
//! [`TmpDir`] owns the directory's lifecycle: created at build start (deleting a stale one left
//! by a killed previous build), removed on drop (best effort) or via [`TmpDir::close`]
//! (reporting errors — the success path).

// Wired into the pipeline by a separate task; until then the lib target sees every item as
// unused. Remove this allow when `pipeline.rs` takes the module up.

use std::fs::{self, File, OpenOptions};
use std::io::{BufReader, BufWriter, Read, Write};
use std::path::{Path, PathBuf};

use crate::error::{BuildError, Result};

/// An entity-indexed `u32` scratch array, backed by a file under `.build-tmp/` instead of the heap.
///
/// **This exists so that a pass bounded by the corpus stops being bounded by RAM.** The build's
/// entity-major scratch — a coordinate axis per entity, and the like — is written once at a random
/// index during one scan and read once at a random index during a later one, and is never sorted.
/// As a `Vec` that is 4 B per entity of *anonymous* memory, which the kernel may not reclaim: 1 GB
/// at 2.5×10⁸ and 4 GB at 10⁹, per array, that a machine must simply have. Mapped, the same bytes
/// are page cache — the kernel keeps what fits and evicts the rest under pressure, so a smaller
/// machine gets slower rather than OOM-killed, and a larger one is no worse off because the pages
/// stay resident anyway.
///
/// **It carries no receipt, unlike the bucket and band files above, and the reason is the
/// lifetime rather than an inconsistency.** Those are written, closed, and read back — a torn or
/// doubly-appended file is a real risk and feeds the permanent entity-ID assignment (I9). This is
/// a mapping held open across its only writer and its only reader in one process: there is no
/// close-and-reopen for a receipt to guard, and its contents are re-derivable from the points file
/// in any case. What it does share is the directory and its lifecycle, so a killed build leaves it
/// to the next `TmpDir::create` exactly like the rest.
#[derive(Debug)]
pub(crate) struct MappedU32 {
    map: memmap2::MmapMut,
    len: usize,
}

impl MappedU32 {
    /// A zeroed array of `len` values at `<dir>/<name>`.
    pub(crate) fn zeroed(dir: &Path, name: &str, len: usize) -> Result<Self> {
        let path = dir.join(name);
        let file = OpenOptions::new()
            .read(true)
            .write(true)
            .create(true)
            .truncate(true)
            .open(&path)
            .map_err(|e| BuildError::io(&path, e))?;
        // A fresh file reads as zeros, which is the initial value every caller wants; `set_len`
        // makes those zeros addressable without writing them.
        let bytes = (len as u64)
            .checked_mul(4)
            .expect("entity count times 4 exceeds u64");
        file.set_len(bytes).map_err(|e| BuildError::io(&path, e))?;
        // SAFETY: the file is this build's own, created empty under a directory it owns, and the
        // mapping is not shared with another process. Its length is fixed for the mapping's life.
        let map = unsafe { memmap2::MmapMut::map_mut(&file) }.map_err(|e| BuildError::io(&path, e))?;
        Ok(MappedU32 { map, len })
    }

    /// The array. Taken once by the caller and held as an ordinary slice for the rest of the pass —
    /// there is no shared-borrow accessor beside it because every caller both fills and reads it,
    /// and one binding for both is what keeps the mapping's exclusivity obvious.
    pub(crate) fn as_mut_slice(&mut self) -> &mut [u32] {
        // SAFETY: `mmap` returns a page-aligned pointer, so `u32` alignment holds; the mapping is
        // `len * 4` bytes by construction; and `&mut self` gives exclusive access to it.
        unsafe { std::slice::from_raw_parts_mut(self.map.as_mut_ptr().cast::<u32>(), self.len) }
    }
}

/// Buffer size for spill I/O, both directions. Four mebibytes: large enough that the syscall
/// cost is noise against the encode/decode work, small enough to be irrelevant against the
/// build's peak memory.
const SPILL_BUF_BYTES: usize = 4 << 20;

/// splitmix64's finalizer — a private **twin of `pipeline::mix64`**, same constants (the ones
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
fn mix64(mut x: u64) -> u64 {
    x = x.wrapping_add(0x9E37_79B9_7F4A_7C15);
    x = (x ^ (x >> 30)).wrapping_mul(0xBF58_476D_1CE4_E5B9);
    x = (x ^ (x >> 27)).wrapping_mul(0x94D0_49BB_1331_11EB);
    x ^ (x >> 31)
}

/// The value a band file's anchor mixes per pair: `term` in the high half, `entity` in the low.
fn pack_pair(term: u32, entity: u32) -> u64 {
    ((term as u64) << 32) | entity as u64
}

/// What `finish` hands back and every read path verifies against: the file, how many records
/// it holds, and the content anchor over those records.
///
/// The receipt lives in the build's memory, never on disk — a receipt stored beside the file
/// it vouches for could be tampered with in the same incident, and the build that wrote the
/// spill is the only reader it will ever have.
#[derive(Debug, Clone)]
pub(crate) struct SpillReceipt {
    pub(crate) path: PathBuf,
    /// Records written: `u64` values for a bucket file, `(term, entity)` pairs for a band file.
    pub(crate) count: u64,
    /// Wrapping sum of [`mix64`] over each record — the raw `u64` for a bucket file,
    /// [`pack_pair`] for a band file.
    pub(crate) anchor: u64,
}

// --------------------------------------------------------------------------------------------
// TmpDir
// --------------------------------------------------------------------------------------------

/// Owns `<out>/.build-tmp/` for the duration of one build.
///
/// **A pre-existing `.build-tmp/` is deleted at creation, not adopted.** The directory is
/// exclusively build-owned transient state: nothing but a running `tessera build` ever writes
/// there, no manifest ever names a file inside it, and its contents are meaningless outside
/// the run that wrote them (their receipts live only in that process's memory). So a directory
/// found at creation can only be the leavings of a previous build that died without cleanup —
/// a `kill -9` mid-run — and deleting it is the correct recovery: adopting stale spill files
/// would be exactly the silent-partial-read failure this module exists to close off, and
/// refusing outright would demand manual cleanup after every crash for no safety gain. If the
/// deletion itself fails (permissions, or `.build-tmp` turns out to be a plain file, which no
/// build ever creates), creation refuses — fail closed, never build atop state we could not
/// clear.
#[derive(Debug)]
pub(crate) struct TmpDir {
    path: PathBuf,
    /// Cleared by [`TmpDir::close`] so `Drop` does not attempt a second removal after the
    /// reported one.
    armed: bool,
}

impl TmpDir {
    /// Create `<bundle_out>/.build-tmp/`, deleting a stale one first (see the type docs).
    pub(crate) fn create(bundle_out: &Path) -> Result<TmpDir> {
        let path = bundle_out.join(".build-tmp");
        // `symlink_metadata` rather than `exists()`: a dangling symlink at the path would make
        // `exists()` say no and `create_dir_all` then fail confusingly; whatever occupies the
        // name, remove it as a tree or refuse.
        if path.symlink_metadata().is_ok() {
            fs::remove_dir_all(&path).map_err(|e| BuildError::io(&path, e))?;
        }
        fs::create_dir_all(&path).map_err(|e| BuildError::io(&path, e))?;
        Ok(TmpDir { path, armed: true })
    }

    pub(crate) fn path(&self) -> &Path {
        &self.path
    }

    /// Remove the directory tree, reporting failure — the success path's cleanup. `Drop`
    /// covers the error paths best-effort, but only `close` can tell the caller that a spill
    /// file was still busy or the filesystem refused.
    pub(crate) fn close(mut self) -> Result<()> {
        self.armed = false;
        fs::remove_dir_all(&self.path).map_err(|e| BuildError::io(&self.path, e))
    }
}

impl Drop for TmpDir {
    fn drop(&mut self) {
        if self.armed {
            // Best effort: an error here means the *build* already failed for some other
            // reason (the success path calls `close`), and the next build's `create` deletes
            // whatever this leaves behind.
            let _ = fs::remove_dir_all(&self.path);
        }
    }
}

// --------------------------------------------------------------------------------------------
// Bucket files
// --------------------------------------------------------------------------------------------

/// Appends one bucket file: packed `u64` values (`ordinal << 32 | term`).
///
/// **On disk:** each value as 8 bytes little-endian, concatenated; no header, no trailer, no
/// padding — `count * 8` bytes exactly. Integrity is external, via the [`SpillReceipt`].
pub(crate) struct SpillWriter {
    path: PathBuf,
    writer: BufWriter<File>,
    count: u64,
    anchor: u64,
}

impl SpillWriter {
    pub(crate) fn create(path: &Path) -> Result<SpillWriter> {
        let file = File::create(path).map_err(|e| BuildError::io(path, e))?;
        Ok(SpillWriter {
            path: path.to_path_buf(),
            writer: BufWriter::with_capacity(SPILL_BUF_BYTES, file),
            count: 0,
            anchor: 0,
        })
    }

    pub(crate) fn push(&mut self, value: u64) -> Result<()> {
        self.writer
            .write_all(&value.to_le_bytes())
            .map_err(|e| BuildError::io(&self.path, e))?;
        self.count += 1;
        self.anchor = self.anchor.wrapping_add(mix64(value));
        Ok(())
    }

    /// Flush, fsync, and hand back the receipt the eventual [`read_bucket`] must be given.
    pub(crate) fn finish(self) -> Result<SpillReceipt> {
        let SpillWriter {
            path,
            writer,
            count,
            anchor,
        } = self;
        let file = writer
            .into_inner()
            .map_err(|e| BuildError::io(&path, e.into_error()))?;
        file.sync_all().map_err(|e| BuildError::io(&path, e))?;
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
pub(crate) fn read_bucket(receipt: &SpillReceipt) -> Result<Vec<u64>> {
    // Checked: `count` comes from our own receipt, but a length computation that can wrap is a
    // length computation that can be made to lie, so the multiply is guarded regardless of
    // provenance. (No `count <= u32::MAX` cap here — callers enforce their own.)
    let expected_bytes = receipt.count.checked_mul(8).ok_or_else(|| {
        BuildError::Invalid(format!(
            "bucket file {}: receipt count {} overflows the byte-length computation",
            receipt.path.display(),
            receipt.count
        ))
    })?;
    let bytes = fs::read(&receipt.path).map_err(|e| BuildError::io(&receipt.path, e))?;
    if bytes.len() as u64 != expected_bytes {
        return Err(BuildError::Invalid(format!(
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
        return Err(BuildError::Invalid(format!(
            "bucket file {}: content anchor mismatch: recomputed {anchor:#018x} but the \
             receipt says {:#018x} — the file's bytes are not the bytes that were written",
            receipt.path.display(),
            receipt.anchor
        )));
    }
    Ok(values)
}

// --------------------------------------------------------------------------------------------
// Band files
// --------------------------------------------------------------------------------------------

/// Appends one band file: a `(term, entity)` pair stream for terms in `[term_lo, ...)`.
///
/// **On disk:** a 4-byte little-endian `term_lo` header, then one record per pair —
/// `varint(term - term_lo) ‖ varint(entity - last_entity)`, both LEB128 (7 payload bits per
/// byte, continuation in the high bit, at most 5 bytes for a `u32`). Integrity is external,
/// via the [`SpillReceipt`]; the header is covered *indirectly* — the anchor mixes the
/// resolved `(term, entity)` pairs, so a corrupted header shifts every decoded term and the
/// anchor check fails.
///
/// **The entity delta is per FILE, not per term.** The emitter walks items in assignment
/// order — entity ids ascend across every push into a given file (equal for one item's
/// several terms, strictly rising between items, and rising across batches because batch
/// bases ascend) — so a single last-entity register per writer suffices, no T-sized state.
/// The measured alternative (absolute entities, ~5 bytes each) put a 10⁹-item corpus's bands
/// at ~39 GB and over the disk; deltas are overwhelmingly one byte. `push` refuses a
/// regressing entity at the write site (an emitter-ordering bug), and per-term ascent is
/// still enforced end-to-end by the band's *consumer* (the pipeline's cursor-scatter feeding
/// `encode_posting`'s sortedness check).
pub(crate) struct BandWriter {
    path: PathBuf,
    writer: BufWriter<File>,
    term_lo: u32,
    last_entity: u32,
    count: u64,
    anchor: u64,
}

impl BandWriter {
    pub(crate) fn create(path: &Path, term_lo: u32) -> Result<BandWriter> {
        let file = File::create(path).map_err(|e| BuildError::io(path, e))?;
        let mut writer = BufWriter::with_capacity(SPILL_BUF_BYTES, file);
        writer
            .write_all(&term_lo.to_le_bytes())
            .map_err(|e| BuildError::io(path, e))?;
        Ok(BandWriter {
            path: path.to_path_buf(),
            writer,
            term_lo,
            last_entity: 0,
            count: 0,
            anchor: 0,
        })
    }

    pub(crate) fn push(&mut self, term: u32, entity: u32) -> Result<()> {
        // A term below the band's floor is unencodable; refusing here keeps the failure at the
        // write site (a routing bug in the emitter) instead of surfacing as a baffling decode
        // error a stage later.
        let delta = term.checked_sub(self.term_lo).ok_or_else(|| {
            BuildError::Invalid(format!(
                "band file {}: term {term} is below the band's term_lo {}",
                self.path.display(),
                self.term_lo
            ))
        })?;
        // The per-file entity register: a regressing entity is an emitter-ordering bug and
        // must fail here, at the write site, not decode into a wrong posting later.
        let entity_delta = entity.checked_sub(self.last_entity).ok_or_else(|| {
            BuildError::Invalid(format!(
                "band file {}: entity {entity} regresses below the file's last entity {}",
                self.path.display(),
                self.last_entity
            ))
        })?;
        write_varint(&mut self.writer, &self.path, delta)?;
        write_varint(&mut self.writer, &self.path, entity_delta)?;
        self.last_entity = entity;
        self.count += 1;
        self.anchor = self.anchor.wrapping_add(mix64(pack_pair(term, entity)));
        Ok(())
    }

    /// Flush, fsync, and hand back the receipt [`BandReader::open`] must be given.
    pub(crate) fn finish(self) -> Result<SpillReceipt> {
        let BandWriter {
            path,
            writer,
            term_lo: _,
            last_entity: _,
            count,
            anchor,
        } = self;
        let file = writer
            .into_inner()
            .map_err(|e| BuildError::io(&path, e.into_error()))?;
        file.sync_all().map_err(|e| BuildError::io(&path, e))?;
        Ok(SpillReceipt {
            path,
            count,
            anchor,
        })
    }
}

/// LEB128-encode `value` into `writer` (at most 5 bytes for a `u32`).
fn write_varint(writer: &mut BufWriter<File>, path: &Path, mut value: u32) -> Result<()> {
    let mut buf = [0u8; 5];
    let mut len = 0;
    loop {
        let byte = (value & 0x7F) as u8;
        value >>= 7;
        if value == 0 {
            buf[len] = byte;
            len += 1;
            break;
        }
        buf[len] = byte | 0x80;
        len += 1;
    }
    writer
        .write_all(&buf[..len])
        .map_err(|e| BuildError::io(path, e))
}

/// Streams a band file back, pair by pair, verifying count and anchor at end of stream.
///
/// The verification is *terminal* by nature — a running sum can only be checked once the
/// stream ends — so a consumer that abandons the reader early has verified nothing. That fits
/// the band lifecycle (each band is decoded sequentially exactly once, to completion); a
/// future partial-read use would need a different design, not a relaxation of this one.
pub(crate) struct BandReader {
    path: PathBuf,
    reader: BufReader<File>,
    term_lo: u32,
    last_entity: u32,
    expect_count: u64,
    expect_anchor: u64,
    count: u64,
    anchor: u64,
    /// Set once end-of-stream verification has passed; further `next` calls return `Ok(None)`.
    done: bool,
}

impl BandReader {
    pub(crate) fn open(receipt: &SpillReceipt) -> Result<BandReader> {
        let file = File::open(&receipt.path).map_err(|e| BuildError::io(&receipt.path, e))?;
        let mut reader = BufReader::with_capacity(SPILL_BUF_BYTES, file);
        let mut header = [0u8; 4];
        if let Err(e) = reader.read_exact(&mut header) {
            return Err(if e.kind() == std::io::ErrorKind::UnexpectedEof {
                BuildError::Invalid(format!(
                    "band file {}: truncated before the 4-byte term_lo header",
                    receipt.path.display()
                ))
            } else {
                BuildError::io(&receipt.path, e)
            });
        }
        Ok(BandReader {
            path: receipt.path.clone(),
            reader,
            term_lo: u32::from_le_bytes(header),
            last_entity: 0,
            expect_count: receipt.count,
            expect_anchor: receipt.anchor,
            count: 0,
            anchor: 0,
            done: false,
        })
    }

    /// The next `(term, entity)` pair, or `Ok(None)` at a *verified* end of stream. Every
    /// malformation is an error: a varint truncated mid-record, a term delta overflowing
    /// `u32`, more records than the receipt's count (trailing data), fewer (truncation at a
    /// record boundary), or a content-anchor mismatch.
    #[allow(clippy::should_implement_trait)] // fallible streaming next: `Result<Option<_>>`, not `Iterator`
    pub(crate) fn next(&mut self) -> Result<Option<(u32, u32)>> {
        if self.done {
            return Ok(None);
        }
        // EOF is legitimate only on a record boundary — before a record's first byte.
        let first = match next_byte(&mut self.reader).map_err(|e| BuildError::io(&self.path, e))? {
            None => {
                self.verify_end()?;
                return Ok(None);
            }
            Some(byte) => byte,
        };
        let delta = self.decode_varint(first)?;
        let entity_delta = {
            let byte = self.require_byte()?;
            self.decode_varint(byte)?
        };
        let term = self.term_lo.checked_add(delta).ok_or_else(|| {
            self.malformed(&format!(
                "term delta {delta} overflows u32 above term_lo {}",
                self.term_lo
            ))
        })?;
        let entity = self.last_entity.checked_add(entity_delta).ok_or_else(|| {
            self.malformed(&format!(
                "entity delta {entity_delta} overflows u32 above {}",
                self.last_entity
            ))
        })?;
        self.last_entity = entity;
        if self.count == self.expect_count {
            // One more decodable record than the receipt promised: trailing data. Caught here
            // rather than at EOF so the error names the actual malformation, not a bare count
            // mismatch — and so garbage that happens to decode never reaches the consumer.
            return Err(self.malformed(&format!(
                "trailing data: more records than the receipt's count {}",
                self.expect_count
            )));
        }
        self.count += 1;
        self.anchor = self.anchor.wrapping_add(mix64(pack_pair(term, entity)));
        Ok(Some((term, entity)))
    }

    /// End-of-stream verification: the decoded stream must match the receipt exactly.
    fn verify_end(&mut self) -> Result<()> {
        if self.count != self.expect_count {
            return Err(self.malformed(&format!(
                "record count mismatch: decoded {} pairs but the receipt says {}",
                self.count, self.expect_count
            )));
        }
        if self.anchor != self.expect_anchor {
            return Err(self.malformed(&format!(
                "content anchor mismatch: recomputed {:#018x} but the receipt says {:#018x} — \
                 the file's bytes are not the bytes that were written",
                self.anchor, self.expect_anchor
            )));
        }
        self.done = true;
        Ok(())
    }

    /// Decode one LEB128 `u32` whose first byte has already been read.
    fn decode_varint(&mut self, first: u8) -> Result<u32> {
        let mut value = (first & 0x7F) as u32;
        if first & 0x80 == 0 {
            return Ok(value);
        }
        let mut shift = 7u32;
        loop {
            let byte = self.require_byte()?;
            if shift == 28 {
                // Fifth byte: only 4 payload bits remain in a u32, and there is no sixth byte.
                if byte & 0xF0 != 0 {
                    return Err(self.malformed("varint overflows u32"));
                }
                return Ok(value | ((byte as u32) << 28));
            }
            value |= ((byte & 0x7F) as u32) << shift;
            if byte & 0x80 == 0 {
                return Ok(value);
            }
            shift += 7;
        }
    }

    fn require_byte(&mut self) -> Result<u8> {
        match next_byte(&mut self.reader).map_err(|e| BuildError::io(&self.path, e))? {
            Some(byte) => Ok(byte),
            None => Err(self.malformed("truncated mid-record")),
        }
    }

    fn malformed(&self, detail: &str) -> BuildError {
        BuildError::Invalid(format!("band file {}: {detail}", self.path.display()))
    }
}

/// One byte from `reader`, or `None` at EOF. Retries `Interrupted` (a bare `read` may see it).
fn next_byte(reader: &mut impl Read) -> std::io::Result<Option<u8>> {
    let mut byte = [0u8; 1];
    loop {
        match reader.read(&mut byte) {
            Ok(0) => return Ok(None),
            Ok(_) => return Ok(Some(byte[0])),
            Err(e) if e.kind() == std::io::ErrorKind::Interrupted => continue,
            Err(e) => return Err(e),
        }
    }
}

// --------------------------------------------------------------------------------------------
// Text-index run files
// --------------------------------------------------------------------------------------------

/// FNV-1a over a term's bytes — the fingerprint a run file's anchor mixes in place of the `u32`
/// term id the band files have.
///
/// A text run's records are keyed by the term *string*: there is no term id yet, because assigning
/// one is what the merge these files feed does. So the anchor cannot use [`pack_pair`], and it
/// folds the key's bytes to 64 bits instead. Not cryptographic, and for the same reason
/// [`mix64`]'s doc gives: this defends against truncation, a torn write and a doubly-appended
/// file, not against an adversary with write access to the build's own scratch.
fn fingerprint(bytes: &[u8]) -> u64 {
    let mut hash = 0xcbf2_9ce4_8422_2325u64;
    for &byte in bytes {
        hash ^= byte as u64;
        hash = hash.wrapping_mul(0x0000_0100_0000_01b3);
    }
    hash
}

/// Appends one **text run**: a sorted, distinct term stream with each term's ascending entity
/// list, spilled by one worker of the text index's chunk pass and consumed once by its merge.
///
/// **On disk:** no header, then one record per term —
/// `varint(shared) ‖ varint(suffix_len) ‖ suffix ‖ varint(count) ‖ varint(entity₀) ‖
/// varint(entityᵢ − entityᵢ₋₁)…`. `shared` is the term's common prefix with its predecessor *in
/// this file*, which is the same front coding the dictionary itself uses and for the same reason:
/// a sorted term stream repeats its prefixes, and the corpus that makes this file large is exactly
/// the one whose terms share them. Integrity is external, via the [`SpillReceipt`].
///
/// **The entity delta is per TERM, not per file**, which is where this differs from
/// [`BandWriter`]. A band file's records ascend in entity across the whole file because its
/// emitter walks items in assignment order; a run's do not — the run is term-major, so entities
/// restart at each term's own first. Both are refused at the write site rather than decoded into a
/// wrong posting later: terms must ascend strictly across the file, entities strictly within a
/// record.
pub(crate) struct TextRunWriter {
    path: PathBuf,
    writer: BufWriter<File>,
    prev: Vec<u8>,
    terms: u64,
    /// Entities still owed for the open term. A record's count is written before its entities, so
    /// a term is only complete when every one it promised has arrived — and `finish` refuses a
    /// file whose last record never was.
    owed: u32,
    /// The last entity written for the open term, for the delta.
    last_entity: u32,
    started: bool,
    /// Records written: `(term, entity)` pairs, so a truncation anywhere in a record shows up.
    count: u64,
    anchor: u64,
    /// [`fingerprint`] of the open term, folded once per record rather than once per pair.
    mark: u64,
}

impl TextRunWriter {
    pub(crate) fn create(path: &Path) -> Result<TextRunWriter> {
        let file = File::create(path).map_err(|e| BuildError::io(path, e))?;
        Ok(TextRunWriter {
            path: path.to_path_buf(),
            writer: BufWriter::with_capacity(SPILL_BUF_BYTES, file),
            prev: Vec::new(),
            terms: 0,
            owed: 0,
            last_entity: 0,
            started: false,
            count: 0,
            anchor: 0,
            mark: 0,
        })
    }

    /// Open a record: the term, and how many entities will follow.
    ///
    /// **Split from [`Self::push_entity`] because a merge cannot hold the slice.** A worker
    /// spilling its own accumulator has the entity list in hand; a cascade pass merging sixty-four
    /// runs into one has a *stream*, and the term it is writing may be carried by a quarter of the
    /// corpus. Buffering that to satisfy a slice signature would reintroduce, in the merge, the
    /// residency the merge exists to bound.
    pub(crate) fn begin(&mut self, term: &[u8], entities: u32) -> Result<()> {
        if self.owed > 0 {
            return Err(BuildError::Invalid(format!(
                "text run {}: term {:?} opens while {} entities are still owed on {:?}",
                self.path.display(),
                String::from_utf8_lossy(term),
                self.owed,
                String::from_utf8_lossy(&self.prev)
            )));
        }
        if self.terms > 0 && term <= self.prev.as_slice() {
            return Err(BuildError::Invalid(format!(
                "text run {}: terms must ascend strictly: {:?} follows {:?}",
                self.path.display(),
                String::from_utf8_lossy(term),
                String::from_utf8_lossy(&self.prev)
            )));
        }
        if term.is_empty() {
            return Err(BuildError::Invalid(format!(
                "text run {}: the empty string is not a term",
                self.path.display()
            )));
        }
        if entities == 0 {
            return Err(BuildError::Invalid(format!(
                "text run {}: term {:?} carries no entity — a term exists in this file only \
                 because something carried it",
                self.path.display(),
                String::from_utf8_lossy(term)
            )));
        }
        let shared = common_prefix_len(&self.prev, term);
        write_varint(&mut self.writer, &self.path, shared as u32)?;
        write_varint(&mut self.writer, &self.path, (term.len() - shared) as u32)?;
        self.writer
            .write_all(&term[shared..])
            .map_err(|e| BuildError::io(&self.path, e))?;
        write_varint(&mut self.writer, &self.path, entities)?;

        self.mark = fingerprint(term);
        self.prev.clear();
        self.prev.extend_from_slice(term);
        self.terms += 1;
        self.owed = entities;
        self.started = false;
        self.last_entity = 0;
        Ok(())
    }

    /// Append one entity to the open record. Entities must ascend strictly within a term — refused
    /// here, at the write site, rather than decoded into a wrong posting a stage later.
    pub(crate) fn push_entity(&mut self, entity: u32) -> Result<()> {
        if self.owed == 0 {
            return Err(BuildError::Invalid(format!(
                "text run {}: entity {entity} arrives with no open term",
                self.path.display()
            )));
        }
        let delta = if self.started {
            entity
                .checked_sub(self.last_entity)
                .filter(|d| *d > 0)
                .ok_or_else(|| {
                    BuildError::Invalid(format!(
                        "text run {}: term {:?}'s entities must ascend strictly: {entity} \
                         follows {}",
                        self.path.display(),
                        String::from_utf8_lossy(&self.prev),
                        self.last_entity
                    ))
                })?
        } else {
            entity
        };
        write_varint(&mut self.writer, &self.path, delta)?;
        self.last_entity = entity;
        self.started = true;
        self.owed -= 1;
        self.count += 1;
        self.anchor = self.anchor.wrapping_add(mix64(self.mark ^ entity as u64));
        Ok(())
    }

    /// One whole record — [`Self::begin`] and its entities — for the caller that holds the list.
    /// Written in terms of the streaming pair so there is one encoding rule rather than two that
    /// could drift.
    pub(crate) fn push(&mut self, term: &[u8], entities: &[u32]) -> Result<()> {
        let count = u32::try_from(entities.len()).map_err(|_| {
            BuildError::Invalid(format!(
                "text run {}: term {:?} carries more entities than a u32 can count",
                self.path.display(),
                String::from_utf8_lossy(term)
            ))
        })?;
        self.begin(term, count)?;
        for &entity in entities {
            self.push_entity(entity)?;
        }
        Ok(())
    }

    /// Flush, fsync, and hand back the receipt [`TextRunReader::open`] must be given.
    pub(crate) fn finish(self) -> Result<SpillReceipt> {
        if self.owed > 0 {
            return Err(BuildError::Invalid(format!(
                "text run {}: {} entities are still owed on term {:?}",
                self.path.display(),
                self.owed,
                String::from_utf8_lossy(&self.prev)
            )));
        }
        let TextRunWriter {
            path,
            writer,
            count,
            anchor,
            ..
        } = self;
        let file = writer
            .into_inner()
            .map_err(|e| BuildError::io(&path, e.into_error()))?;
        file.sync_all().map_err(|e| BuildError::io(&path, e))?;
        Ok(SpillReceipt {
            path,
            count,
            anchor,
        })
    }
}

fn common_prefix_len(a: &[u8], b: &[u8]) -> usize {
    a.iter().zip(b).take_while(|(x, y)| x == y).count()
}

/// Streams a text run back **term by term**, verifying count and anchor at end of stream.
///
/// **The head's entities are not decoded until they are asked for.** A k-way merge holds one open
/// reader per run and compares only their head terms, so a reader that eagerly materialised each
/// head's entity list would put *k* whole posting lists in memory to choose between *k* strings —
/// which is the residency this whole pass exists to bound. [`Self::advance`] reads the term and
/// its length and stops; [`Self::take_entities`] decodes them, and `advance` drains whatever a
/// caller left rather than trusting it to have consumed the record.
///
/// Verification is terminal, exactly as [`BandReader`]'s is and for the same reason — a running
/// sum can only be checked once the stream ends — and the merge drains every run to completion.
pub(crate) struct TextRunReader {
    path: PathBuf,
    reader: BufReader<File>,
    /// The head term, front-decoded against its predecessor.
    term: Vec<u8>,
    /// [`fingerprint`] of `term`, held so the anchor costs one fold per record and not one per
    /// pair.
    mark: u64,
    /// Entities of the head not yet decoded.
    pending: u32,
    /// Entities of the head already decoded — what makes the first delta absolute and the rest
    /// relative, and it cannot be inferred from `last_entity` because entity 0 is a legal first.
    taken: u32,
    /// The last entity decoded within the head, for the delta.
    last_entity: u32,
    expect_count: u64,
    expect_anchor: u64,
    count: u64,
    anchor: u64,
    done: bool,
}

impl TextRunReader {
    pub(crate) fn open(receipt: &SpillReceipt) -> Result<TextRunReader> {
        let file = File::open(&receipt.path).map_err(|e| BuildError::io(&receipt.path, e))?;
        Ok(TextRunReader {
            path: receipt.path.clone(),
            reader: BufReader::with_capacity(SPILL_BUF_BYTES, file),
            term: Vec::new(),
            mark: 0,
            pending: 0,
            taken: 0,
            last_entity: 0,
            expect_count: receipt.count,
            expect_anchor: receipt.anchor,
            count: 0,
            anchor: 0,
            done: false,
        })
    }

    /// Move to the next term, returning `false` at a *verified* end of stream.
    pub(crate) fn advance(&mut self) -> Result<bool> {
        if self.done {
            return Ok(false);
        }
        // Whatever the caller did not take: decoded and anchored, never skipped by seeking, so a
        // malformation inside a record the merge had no use for is still caught.
        while self.pending > 0 {
            self.next_entity()?;
        }
        let first = match next_byte(&mut self.reader).map_err(|e| BuildError::io(&self.path, e))? {
            None => {
                self.verify_end()?;
                return Ok(false);
            }
            Some(byte) => byte,
        };
        let shared = self.decode_varint(first)? as usize;
        if shared > self.term.len() {
            return Err(self.malformed(&format!(
                "shared prefix {shared} exceeds the previous term's {} bytes",
                self.term.len()
            )));
        }
        let suffix_len = {
            let byte = self.require_byte()?;
            self.decode_varint(byte)? as usize
        };
        self.term.truncate(shared);
        // Read through `take` and grow rather than `resize`-then-`read_exact`: `suffix_len` comes
        // off the file, and a corrupted one would otherwise be an allocation of whatever it says
        // before a single byte is checked against what the file actually holds.
        let got = (&mut self.reader)
            .take(suffix_len as u64)
            .read_to_end(&mut self.term)
            .map_err(|e| BuildError::io(&self.path, e))?;
        if got != suffix_len {
            return Err(self.malformed("truncated inside a term's bytes"));
        }
        if self.term.is_empty() {
            return Err(self.malformed("the empty string is not a term"));
        }
        // Refused rather than decoded: the writer refuses a zero-entity record, so one here is a
        // malformed file and not a term that lost its postings.
        let count = {
            let byte = self.require_byte()?;
            self.decode_varint(byte)?
        };
        if count == 0 {
            return Err(self.malformed("a term with no entities"));
        }
        if self.count + count as u64 > self.expect_count {
            return Err(self.malformed(&format!(
                "trailing data: more pairs than the receipt's count {}",
                self.expect_count
            )));
        }
        self.mark = fingerprint(&self.term);
        self.pending = count;
        self.taken = 0;
        self.last_entity = 0;
        Ok(true)
    }

    /// The head term. Meaningful only after [`Self::advance`] returned `true`.
    pub(crate) fn term(&self) -> &[u8] {
        &self.term
    }

    /// How many entities the head carries, decoded or not.
    pub(crate) fn pending(&self) -> u32 {
        self.pending
    }

    /// Decode the head's remaining entities into `sink`, ascending.
    pub(crate) fn take_entities(&mut self, sink: &mut impl FnMut(u32) -> Result<()>) -> Result<()> {
        while self.pending > 0 {
            let entity = self.next_entity()?;
            sink(entity)?;
        }
        Ok(())
    }

    fn next_entity(&mut self) -> Result<u32> {
        let byte = self.require_byte()?;
        let delta = self.decode_varint(byte)?;
        let entity = if self.taken == 0 {
            delta
        } else {
            if delta == 0 {
                return Err(self.malformed("an entity delta of 0 repeats an entity"));
            }
            let last = self.last_entity;
            last.checked_add(delta).ok_or_else(|| {
                self.malformed(&format!("entity delta {delta} overflows u32 above {last}"))
            })?
        };
        self.last_entity = entity;
        self.taken += 1;
        self.pending -= 1;
        self.count += 1;
        self.anchor = self.anchor.wrapping_add(mix64(self.mark ^ entity as u64));
        Ok(entity)
    }

    fn verify_end(&mut self) -> Result<()> {
        if self.count != self.expect_count {
            return Err(self.malformed(&format!(
                "pair count mismatch: decoded {} pairs but the receipt says {}",
                self.count, self.expect_count
            )));
        }
        if self.anchor != self.expect_anchor {
            return Err(self.malformed(&format!(
                "content anchor mismatch: recomputed {:#018x} but the receipt says {:#018x} — \
                 the file's bytes are not the bytes that were written",
                self.anchor, self.expect_anchor
            )));
        }
        self.done = true;
        Ok(())
    }

    fn decode_varint(&mut self, first: u8) -> Result<u32> {
        let mut value = (first & 0x7F) as u32;
        if first & 0x80 == 0 {
            return Ok(value);
        }
        let mut shift = 7u32;
        loop {
            let byte = self.require_byte()?;
            if shift == 28 {
                if byte & 0xF0 != 0 {
                    return Err(self.malformed("varint overflows u32"));
                }
                return Ok(value | ((byte as u32) << 28));
            }
            value |= ((byte & 0x7F) as u32) << shift;
            if byte & 0x80 == 0 {
                return Ok(value);
            }
            shift += 7;
        }
    }

    fn require_byte(&mut self) -> Result<u8> {
        match next_byte(&mut self.reader).map_err(|e| BuildError::io(&self.path, e))? {
            Some(byte) => Ok(byte),
            None => Err(self.malformed("truncated mid-record")),
        }
    }

    fn malformed(&self, detail: &str) -> BuildError {
        BuildError::Invalid(format!("text run {}: {detail}", self.path.display()))
    }
}

// --------------------------------------------------------------------------------------------
// Tests
// --------------------------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use proptest::prelude::*;

    fn write_bucket(path: &Path, values: &[u64]) -> SpillReceipt {
        let mut writer = SpillWriter::create(path).unwrap();
        for &value in values {
            writer.push(value).unwrap();
        }
        writer.finish().unwrap()
    }

    fn write_band(path: &Path, term_lo: u32, pairs: &[(u32, u32)]) -> SpillReceipt {
        let mut writer = BandWriter::create(path, term_lo).unwrap();
        for &(term, entity) in pairs {
            writer.push(term, entity).unwrap();
        }
        writer.finish().unwrap()
    }

    fn read_band(receipt: &SpillReceipt) -> Result<Vec<(u32, u32)>> {
        let mut reader = BandReader::open(receipt)?;
        let mut out = Vec::new();
        while let Some(pair) = reader.next()? {
            out.push(pair);
        }
        Ok(out)
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

    fn err_string(result: Result<impl std::fmt::Debug>) -> String {
        result.expect_err("expected an error").to_string()
    }

    /// Pins the constants against `pipeline::mix64`'s (both are splitmix64's finalizer): the
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

    #[test]
    fn band_round_trips() {
        let temp = tempfile::TempDir::new().unwrap();
        let cases: Vec<(u32, Vec<(u32, u32)>)> = vec![
            (0, vec![]),
            (0, vec![(0, 0)]),
            (7, vec![(7, 123)]),
            // Terms interleave arbitrarily; entities are NON-DECREASING per file — the
            // emitter's assignment-order contract, which the per-file delta encoding bakes
            // into the format itself (equal entities for one item's several terms, rising
            // between items).
            (
                3,
                vec![(5, 1), (3, 1), (5, 1), (4, 7), (3, 9), (5, 900_000)],
            ),
            // Boundaries: maximal term delta (5-byte varint), maximal entity delta from 0,
            // equal-entity runs at the ceiling.
            (0, vec![(u32::MAX, 0), (0, u32::MAX)]),
            (u32::MAX, vec![(u32::MAX, 0), (u32::MAX, u32::MAX)]),
            (
                100,
                (0..5_000u32).map(|i| (100 + (i % 64), i * 811)).collect(),
            ),
        ];
        for (i, (term_lo, pairs)) in cases.iter().enumerate() {
            let path = temp.path().join(format!("band-{i}.bin"));
            let receipt = write_band(&path, *term_lo, pairs);
            assert_eq!(receipt.count, pairs.len() as u64);
            assert_eq!(read_band(&receipt).unwrap(), *pairs);
        }
    }

    #[test]
    fn band_next_after_verified_end_stays_none() {
        let temp = tempfile::TempDir::new().unwrap();
        let path = temp.path().join("band.bin");
        let receipt = write_band(&path, 2, &[(2, 10), (3, 11)]);
        let mut reader = BandReader::open(&receipt).unwrap();
        while reader.next().unwrap().is_some() {}
        assert_eq!(reader.next().unwrap(), None);
    }

    #[test]
    fn band_refuses_a_term_below_term_lo() {
        let temp = tempfile::TempDir::new().unwrap();
        let path = temp.path().join("band.bin");
        let mut writer = BandWriter::create(&path, 10).unwrap();
        let message = err_string(writer.push(9, 0).map(|_| ()));
        assert!(
            message.contains("below the band's term_lo"),
            "got: {message}"
        );
    }

    #[test]
    fn band_body_flip_is_an_error() {
        let temp = tempfile::TempDir::new().unwrap();
        let pairs: Vec<(u32, u32)> = (0..200u32).map(|i| (50 + (i % 9), i * 3_000_017)).collect();
        // Flip every body byte in turn: whatever the flip does — reshapes a varint, changes a
        // value, sets a stray continuation bit — the read must fail, never silently differ.
        let reference = fs::read({
            let path = temp.path().join("band-ref.bin");
            write_band(&path, 50, &pairs);
            path
        })
        .unwrap();
        for index in 4..reference.len() {
            let path = temp.path().join("band.bin");
            let receipt = write_band(&path, 50, &pairs);
            flip_byte(&path, index);
            assert!(
                read_band(&receipt).is_err(),
                "flipping byte {index} went undetected"
            );
        }
    }

    #[test]
    fn band_header_flip_is_an_error() {
        let temp = tempfile::TempDir::new().unwrap();
        let path = temp.path().join("band.bin");
        let receipt = write_band(&path, 50, &[(50, 1), (51, 2)]);
        // The header is covered indirectly: a shifted term_lo shifts every decoded term, so
        // the anchor (or an overflow check) fails even though every varint still parses.
        flip_byte(&path, 0);
        assert!(read_band(&receipt).is_err());
    }

    #[test]
    fn band_truncation_is_an_error() {
        let temp = tempfile::TempDir::new().unwrap();
        let pairs = &[(5u32, 300u32), (6, 70_000), (5, 1_000_000)];
        let path = temp.path().join("band.bin");
        let full = {
            write_band(&path, 5, pairs);
            fs::read(&path).unwrap().len()
        };
        // Cut at every length from "header only missing one record" down to mid-varint: a
        // record-boundary cut is a count mismatch, an intra-record cut is a truncation error.
        for keep in 4..full {
            let receipt = write_band(&path, 5, pairs);
            truncate_by(&path, full - keep);
            assert!(
                read_band(&receipt).is_err(),
                "truncating to {keep} bytes went undetected"
            );
        }
    }

    #[test]
    fn band_truncated_before_the_header_is_an_error() {
        let temp = tempfile::TempDir::new().unwrap();
        let path = temp.path().join("band.bin");
        let receipt = write_band(&path, 5, &[(5, 1)]);
        fs::write(&path, [0u8, 0]).unwrap();
        let message = err_string(BandReader::open(&receipt).map(|_| ()));
        assert!(message.contains("term_lo header"), "got: {message}");
    }

    #[test]
    fn band_trailing_garbage_is_an_error() {
        let temp = tempfile::TempDir::new().unwrap();
        let path = temp.path().join("band.bin");
        let receipt = write_band(&path, 0, &[(1, 2)]);
        // A complete, well-formed extra record: caught as trailing data by count.
        append(&path, &[0x00, 0x00]);
        let message = err_string(read_band(&receipt));
        assert!(message.contains("trailing data"), "got: {message}");
        // A ragged byte that starts a varint and hits EOF: caught as a truncated record.
        let receipt = write_band(&path, 0, &[(1, 2)]);
        append(&path, &[0xFF]);
        let message = err_string(read_band(&receipt));
        assert!(message.contains("truncated mid-record"), "got: {message}");
    }

    #[test]
    fn band_varint_overflow_is_an_error() {
        let temp = tempfile::TempDir::new().unwrap();
        let path = temp.path().join("band.bin");
        let receipt = write_band(&path, 0, &[]);
        // 6-byte-shaped varint: fifth byte carries payload above bit 31 (and a continuation).
        append(&path, &[0xFF, 0xFF, 0xFF, 0xFF, 0xFF, 0x01, 0x00]);
        let message = err_string(read_band(&receipt));
        assert!(message.contains("overflows u32"), "got: {message}");
    }

    proptest! {
        /// The codec is exact over arbitrary interleavings: any sequence of
        /// `(term >= term_lo, entity)` pairs round-trips in order, through the real files.
        #[test]
        fn band_codec_round_trips(
            (term_lo, raw) in any::<u32>().prop_flat_map(|lo| {
                (
                    Just(lo),
                    prop::collection::vec((lo..=u32::MAX, 0u32..=1 << 20), 0..64),
                )
            })
        ) {
            // Entities must be non-decreasing per file (the format's contract): accumulate
            // the generated values as deltas, saturating at the ceiling.
            let mut entity = 0u32;
            let pairs: Vec<(u32, u32)> = raw
                .into_iter()
                .map(|(term, step)| {
                    entity = entity.saturating_add(step);
                    (term, entity)
                })
                .collect();
            let temp = tempfile::TempDir::new().unwrap();
            let path = temp.path().join("band.bin");
            let receipt = write_band(&path, term_lo, &pairs);
            prop_assert_eq!(receipt.count, pairs.len() as u64);
            prop_assert_eq!(read_band(&receipt).unwrap(), pairs);
        }
    }

    // ---- TmpDir -----------------------------------------------------------------------

    #[test]
    fn band_writer_refuses_a_regressing_entity() {
        let temp = tempfile::TempDir::new().unwrap();
        let mut writer = BandWriter::create(&temp.path().join("band.bin"), 0).unwrap();
        writer.push(1, 10).unwrap();
        writer.push(2, 10).unwrap(); // equal is fine (one item, several terms)
        let err = writer.push(1, 9).unwrap_err().to_string();
        assert!(err.contains("regresses"), "{err}");
    }

    #[test]
    fn tmpdir_creates_and_close_removes() {
        let temp = tempfile::TempDir::new().unwrap();
        let tmp = TmpDir::create(temp.path()).unwrap();
        let path = tmp.path().to_path_buf();
        assert!(path.is_dir());
        assert_eq!(path, temp.path().join(".build-tmp"));
        fs::write(path.join("band.bin"), b"transient").unwrap();
        tmp.close().unwrap();
        assert!(!path.exists());
    }

    #[test]
    fn tmpdir_drop_removes_best_effort() {
        let temp = tempfile::TempDir::new().unwrap();
        let path = {
            let tmp = TmpDir::create(temp.path()).unwrap();
            fs::write(tmp.path().join("bucket.u64"), b"transient").unwrap();
            tmp.path().to_path_buf()
        };
        assert!(!path.exists());
    }

    #[test]
    fn tmpdir_deletes_a_stale_directory_at_creation() {
        let temp = tempfile::TempDir::new().unwrap();
        // A kill -9'd previous build: the directory exists and still holds spill files.
        let stale = temp.path().join(".build-tmp");
        fs::create_dir_all(stale.join("nested")).unwrap();
        fs::write(stale.join("nested").join("band-0.bin"), b"stale").unwrap();
        let tmp = TmpDir::create(temp.path()).unwrap();
        assert!(tmp.path().is_dir());
        assert!(
            !tmp.path().join("nested").exists(),
            "stale contents must be gone"
        );
        tmp.close().unwrap();
    }

    #[test]
    fn tmpdir_refuses_a_plain_file_it_cannot_clear() {
        let temp = tempfile::TempDir::new().unwrap();
        // No build ever creates `.build-tmp` as a file, so this is not ours to delete as a
        // tree — creation must refuse rather than build atop unexplained state.
        fs::write(temp.path().join(".build-tmp"), b"not a directory").unwrap();
        assert!(TmpDir::create(temp.path()).is_err());
    }

    // ---- text run files ---------------------------------------------------------------

    fn write_text_run(path: &Path, records: &[(&str, Vec<u32>)]) -> SpillReceipt {
        let mut writer = TextRunWriter::create(path).unwrap();
        for (term, entities) in records {
            writer.push(term.as_bytes(), entities).unwrap();
        }
        writer.finish().unwrap()
    }

    fn read_text_run(receipt: &SpillReceipt) -> Result<Vec<(String, Vec<u32>)>> {
        let mut reader = TextRunReader::open(receipt)?;
        let mut out = Vec::new();
        while reader.advance()? {
            let term = String::from_utf8(reader.term().to_vec()).unwrap();
            let expected = reader.pending();
            let mut entities = Vec::new();
            reader.take_entities(&mut |entity| {
                entities.push(entity);
                Ok(())
            })?;
            assert_eq!(
                entities.len() as u32,
                expected,
                "`pending` promised the count"
            );
            out.push((term, entities));
        }
        Ok(out)
    }

    #[test]
    fn text_run_round_trips() {
        let temp = tempfile::TempDir::new().unwrap();
        let long: Vec<(&str, Vec<u32>)> = vec![
            // Entity 0 first, which is where an absolute-vs-delta confusion in the first slot
            // would show: a decoder that treated it as a delta from a non-zero register reads a
            // different entity, and every posting after it shifts.
            ("aardvark", vec![0]),
            ("aardvark2", vec![0, 1, 2, 63, 64, 65, u32::MAX]),
            // Shares a long prefix with its predecessor — the front coding's own case.
            ("aardvark2b", vec![7]),
            ("zebra", (0..5_000u32).map(|e| e * 3).collect()),
            ("日本語", vec![1, 2]),
        ];
        let cases: Vec<Vec<(&str, Vec<u32>)>> = vec![vec![], vec![("only", vec![9])], long];
        for (i, records) in cases.iter().enumerate() {
            let path = temp.path().join(format!("text-run-{i}.spill"));
            let receipt = write_text_run(&path, records);
            assert_eq!(
                receipt.count,
                records.iter().map(|(_, e)| e.len() as u64).sum::<u64>()
            );
            let read = read_text_run(&receipt).unwrap();
            let want: Vec<(String, Vec<u32>)> = records
                .iter()
                .map(|(t, e)| (t.to_string(), e.clone()))
                .collect();
            assert_eq!(read, want);
        }
    }

    #[test]
    fn text_run_flip_is_an_anchor_mismatch() {
        let temp = tempfile::TempDir::new().unwrap();
        let path = temp.path().join("text-run.spill");
        let records = vec![("alpha", vec![1, 5, 9]), ("bravo", vec![2, 4])];
        let receipt = write_text_run(&path, &records);
        // The last byte is an entity delta: flipping it changes a posting and nothing else, which
        // is exactly the failure a count check alone would miss.
        let last = fs::read(&path).unwrap().len() - 1;
        flip_byte(&path, last);
        let message = err_string(read_text_run(&receipt));
        assert!(message.contains("anchor mismatch"), "got: {message}");
        assert!(message.contains("text-run.spill"), "got: {message}");
    }

    #[test]
    fn text_run_truncation_is_caught() {
        let temp = tempfile::TempDir::new().unwrap();
        let path = temp.path().join("text-run.spill");
        let records = vec![("alpha", vec![1, 5, 9]), ("bravo", vec![2, 4])];
        for cut in [1usize, 2, 3, 4, 5] {
            let receipt = write_text_run(&path, &records);
            truncate_by(&path, cut);
            let message = err_string(read_text_run(&receipt));
            assert!(
                message.contains("truncated") || message.contains("count mismatch"),
                "cut {cut} got: {message}"
            );
        }
    }

    #[test]
    fn text_run_trailing_data_is_caught() {
        let temp = tempfile::TempDir::new().unwrap();
        let path = temp.path().join("text-run.spill");
        let records = vec![("alpha", vec![1, 5, 9])];
        let receipt = write_text_run(&path, &records);
        // A whole extra record, well formed: `zz` with one entity. Only the receipt's pair count
        // separates it from a legitimate stream.
        append(&path, &[0, 2, b'z', b'z', 1, 3]);
        let message = err_string(read_text_run(&receipt));
        assert!(message.contains("trailing data"), "got: {message}");
    }

    #[test]
    fn text_run_refuses_an_emitter_that_does_not_ascend() {
        let temp = tempfile::TempDir::new().unwrap();
        // A writer apiece: a refused push may already have written a record's header, so the file
        // one leaves behind is only ever discarded — which is what a build that fails here does.
        let refused: Vec<(&str, &[u8], &[u32])> = vec![
            ("a term that regresses", b"alpha", &[3]),
            ("a term that repeats", b"bravo", &[3]),
            ("a term with no entity", b"charlie", &[]),
            ("an entity that repeats", b"charlie", &[5, 5]),
            ("an entity that regresses", b"charlie", &[5, 4]),
        ];
        for (i, (what, term, entities)) in refused.into_iter().enumerate() {
            let path = temp.path().join(format!("refused-{i}.spill"));
            let mut writer = TextRunWriter::create(&path).unwrap();
            writer.push(b"bravo", &[1, 2]).unwrap();
            assert!(writer.push(term, entities).is_err(), "{what}");
        }
    }

    proptest! {
        /// Any sorted, distinct term stream with ascending entity lists round-trips exactly.
        #[test]
        fn any_text_run_round_trips(
            raw in prop::collection::vec(
                (prop::collection::vec(prop::num::u8::ANY, 1..6),
                 prop::collection::vec(prop::num::u32::ANY, 1..8)),
                0..40),
        ) {
            let mut records: Vec<(Vec<u8>, Vec<u32>)> = raw
                .into_iter()
                .map(|(term, mut entities)| {
                    entities.sort_unstable();
                    entities.dedup();
                    (term, entities)
                })
                .collect();
            records.sort_by(|a, b| a.0.cmp(&b.0));
            records.dedup_by(|a, b| a.0 == b.0);

            let temp = tempfile::TempDir::new().unwrap();
            let path = temp.path().join("text-run.spill");
            let mut writer = TextRunWriter::create(&path).unwrap();
            for (term, entities) in &records {
                writer.push(term, entities).unwrap();
            }
            let receipt = writer.finish().unwrap();

            let mut reader = TextRunReader::open(&receipt).unwrap();
            let mut read: Vec<(Vec<u8>, Vec<u32>)> = Vec::new();
            while reader.advance().unwrap() {
                let term = reader.term().to_vec();
                let mut entities = Vec::new();
                reader
                    .take_entities(&mut |entity| {
                        entities.push(entity);
                        Ok(())
                    })
                    .unwrap();
                read.push((term, entities));
            }
            prop_assert_eq!(read, records);
        }
    }
}
