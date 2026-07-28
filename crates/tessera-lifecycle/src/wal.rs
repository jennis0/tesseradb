//! The write-ahead log (lifecycle design §4).
//!
//! Ack contract (wired in Task 13): **WAL append → fsync → in-memory apply/swap → 200.** Never
//! ack before fsync — everything below exists to make that fsync boundary the one place acked
//! state can be trusted from, and everything past it disposable.
//!
//! Record framing on disk: `u32 LE len ‖ postcard bytes ‖ u32 LE crc32(postcard bytes)`.
//!
//! ## The positional CRC rule
//!
//! A crash can leave a torn write at the tail of the log — bytes the OS had buffered but never
//! flushed. Those records were never acked (the ack contract fsyncs first), so silently
//! discarding them on replay is correct: the caller who was waiting on that append never got a
//! 200, and will retry.
//!
//! But a torn/corrupted record *before* the last fsync point is a different failure entirely: it
//! means bytes we told a caller were durable — possibly a `delete` or `suppress` — are damaged.
//! There is no safe repair for that (skipping it would silently un-deny something), so replay
//! must refuse to come up at all.
//!
//! The two cases are told apart by comparing a failing record's **start offset** against the
//! last-fsynced offset, tracked in the sidecar file `wal.sync` (8-byte LE offset, written and
//! fsynced immediately after every WAL fsync — see [`Wal::fsync`]):
//!
//! - failing record starts **at or past** the sync offset → it was never fsynced, so it can only
//!   be an artefact of a torn tail write. Truncate the log there and replay succeeds with the
//!   records collected so far.
//! - failing record starts **before** the sync offset → it lies inside bytes that were reported
//!   durable. Fail closed: return [`WalError::WalCorruption`] and the caller must not become
//!   ready.
use std::fs::{File, OpenOptions};
use std::io::{Read, Seek, SeekFrom, Write};
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

use tessera_types::EntityId;

/// One declared-scalar value carried by a WAL row.
///
/// Mirrors `tessera_spatial::tiler::ScalarValue`'s three Phase 1 kinds. Duplicated rather than
/// imported: the brief scopes this crate's dependencies to `tessera-types`, `postcard` and
/// `crc32fast` only (no `tessera-spatial`), so the WAL carries its own copy of the (tiny, stable)
/// shape. Keep the two enums in lockstep if either changes.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub enum WalScalar {
    U64(u64),
    F32(f32),
    Utf8(String),
}

/// One item within an `IngestBatch` record.
///
/// Carries its already-**allocated** entity ID (SA §6.2): the allocator assigns IDs before the
/// batch is framed into the WAL, and replay must reuse exactly those IDs rather than
/// re-allocating — re-allocating on replay would silently reorder or duplicate entity space.
///
/// `descriptors` are raw term-descriptor bytes, never `TermId`s: term IDs are bundle-relative
/// ordinals fixed by the bundle's (immutable) dictionary extents, so a term coined between
/// builds has no durable ID yet. Descriptors resolve through the bundle dictionary plus a
/// deterministic in-memory extension interned in replay order at load time.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct WalRow {
    pub external_id: Vec<u8>,
    pub entity_id: EntityId,
    pub descriptors: Vec<Vec<u8>>,
    pub x: f32,
    pub y: f32,
    pub scalars: Vec<WalScalar>,
}

/// The disposition change carried by a `Change` record. The three retirement rules (lifecycle
/// §3) are distinct and must not be conflated: deletion denies retire by the epoch ledger,
/// suppressions retire only on `Unsuppress` (never touching postings), and predicate changes
/// retire at their compaction fold.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum ChangeOp {
    Predicate,
    Delete,
    Suppress,
    Unsuppress,
}

/// One framed WAL record.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub enum WalRecord {
    /// An accepted `/control/ingest` batch. `body_hash` is the SHA-256 of the raw request body
    /// (idempotency key material — a retried `batch_id` must match it, or the request is a
    /// contract violation, never a silent overwrite).
    IngestBatch {
        batch_id: String,
        body_hash: [u8; 32],
        rows: Vec<WalRow>,
    },
    /// An accepted `/control/changes` entry. `descriptors` carries the new predicate's term
    /// descriptors for `Predicate` changes; `None` for `Delete`/`Suppress`/`Unsuppress`, which
    /// change disposition without touching terms.
    Change {
        external_id: Vec<u8>,
        op: ChangeOp,
        descriptors: Option<Vec<Vec<u8>>>,
    },
    /// A durable reservation of an entity-ID range, written before the range's rows are known to
    /// exist so a crash mid-batch cannot let a later batch reuse the reserved IDs (I9).
    Lease { lo: u64, hi: u64 },
}

/// WAL-level failures. [`WalError::WalCorruption`] is the fail-closed case: the caller must not
/// treat the WAL as open/ready.
#[derive(Debug)]
pub enum WalError {
    Io(std::io::Error),
    Postcard(postcard::Error),
    /// Framing/CRC failure starting before the last-fsynced offset — acked state, possibly a
    /// deny, is damaged. Fail closed (lifecycle design §4): do not come ready.
    WalCorruption,
}

impl std::fmt::Display for WalError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            WalError::Io(e) => write!(f, "wal io error: {e}"),
            WalError::Postcard(e) => write!(f, "wal encoding error: {e}"),
            WalError::WalCorruption => write!(
                f,
                "wal corruption before the last-fsynced offset — acked state may be damaged"
            ),
        }
    }
}

impl std::error::Error for WalError {}

impl From<std::io::Error> for WalError {
    fn from(e: std::io::Error) -> Self {
        WalError::Io(e)
    }
}

impl From<postcard::Error> for WalError {
    fn from(e: postcard::Error) -> Self {
        WalError::Postcard(e)
    }
}

pub type Result<T> = std::result::Result<T, WalError>;

/// An open write-ahead log. `open` replays existing records; `append` buffers a new one;
/// `fsync` is the durability boundary the ack contract waits on.
pub struct Wal {
    file: File,
    sync_path: PathBuf,
    /// Current end-of-file offset — bytes appended so far, whether or not yet fsynced.
    len: u64,
}

fn sync_sidecar_path(wal_path: &Path) -> PathBuf {
    match wal_path.parent() {
        Some(dir) if !dir.as_os_str().is_empty() => dir.join("wal.sync"),
        _ => PathBuf::from("wal.sync"),
    }
}

/// Reads the sidecar's last-fsync offset. A missing sidecar means nothing has ever been
/// fsynced (offset 0) — a fresh WAL, or one that crashed before its first fsync. A sidecar that
/// exists but is not exactly 8 bytes cannot be trusted; treat it the same as "nothing synced"
/// (offset 0), which is the conservative choice: it makes replay more likely to fail closed on
/// any subsequent framing problem, never less.
fn read_sync_point(sync_path: &Path) -> Result<u64> {
    match std::fs::read(sync_path) {
        Ok(bytes) if bytes.len() == 8 => {
            let mut buf = [0u8; 8];
            buf.copy_from_slice(&bytes);
            Ok(u64::from_le_bytes(buf))
        }
        Ok(_) => Ok(0),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(0),
        Err(e) => Err(e.into()),
    }
}

/// Reads up to `buf.len()` bytes, stopping at EOF. Returns the number of bytes actually read,
/// which is less than `buf.len()` iff EOF was reached before the buffer was filled.
fn read_up_to(file: &mut File, buf: &mut [u8]) -> std::io::Result<usize> {
    let mut total = 0;
    while total < buf.len() {
        let n = file.read(&mut buf[total..])?;
        if n == 0 {
            break;
        }
        total += n;
    }
    Ok(total)
}

/// Replays every record from offset 0, applying the positional CRC rule on the first framing or
/// CRC failure encountered. Returns the records collected and the offset replay stopped at
/// (== the file's logical length after any truncation).
fn replay(file: &mut File, sync_point: u64) -> Result<(Vec<WalRecord>, u64)> {
    file.seek(SeekFrom::Start(0))?;
    let mut records = Vec::new();
    let mut pos: u64 = 0;

    loop {
        let mut len_buf = [0u8; 4];
        let n = read_up_to(file, &mut len_buf)?;
        if n == 0 {
            // Clean end of log: nothing was ever written from `pos` onward.
            break;
        }
        if n < 4 {
            return finish_on_failure(file, pos, sync_point, records);
        }
        let body_len = u32::from_le_bytes(len_buf) as usize;

        let mut body = vec![0u8; body_len];
        if read_up_to(file, &mut body)? < body_len {
            return finish_on_failure(file, pos, sync_point, records);
        }

        let mut crc_buf = [0u8; 4];
        if read_up_to(file, &mut crc_buf)? < 4 {
            return finish_on_failure(file, pos, sync_point, records);
        }
        let stored_crc = u32::from_le_bytes(crc_buf);
        let actual_crc = crc32fast::hash(&body);
        if actual_crc != stored_crc {
            return finish_on_failure(file, pos, sync_point, records);
        }

        let record: WalRecord = match postcard::from_bytes(&body) {
            Ok(r) => r,
            Err(_) => return finish_on_failure(file, pos, sync_point, records),
        };

        records.push(record);
        pos += 4 + body_len as u64 + 4;
    }

    Ok((records, pos))
}

/// Applies the positional CRC rule at the failing record's start offset `pos`.
fn finish_on_failure(
    file: &mut File,
    pos: u64,
    sync_point: u64,
    records: Vec<WalRecord>,
) -> Result<(Vec<WalRecord>, u64)> {
    if pos < sync_point {
        // Inside the region we told a caller was durable. Do not repair by discarding it.
        Err(WalError::WalCorruption)
    } else {
        // At or past the sync point: never acked. Safe to discard silently.
        file.set_len(pos)?;
        Ok((records, pos))
    }
}

impl Wal {
    /// Opens (creating if absent) the WAL at `path`, replays it under the positional CRC rule,
    /// and returns the live handle plus every record recovered.
    pub fn open<P: AsRef<Path>>(path: P) -> Result<(Wal, Vec<WalRecord>)> {
        let path = path.as_ref();
        let sync_path = sync_sidecar_path(path);
        let sync_point = read_sync_point(&sync_path)?;

        let mut file = OpenOptions::new()
            .create(true)
            .truncate(false)
            .read(true)
            .write(true)
            .open(path)?;

        let (records, len) = replay(&mut file, sync_point)?;
        file.seek(SeekFrom::Start(len))?;

        Ok((
            Wal {
                file,
                sync_path,
                len,
            },
            records,
        ))
    }

    /// Buffers `rec` for append. Not durable until [`Wal::fsync`] returns.
    pub fn append(&mut self, rec: &WalRecord) -> Result<()> {
        let body = postcard::to_allocvec(rec)?;
        let crc = crc32fast::hash(&body);
        let len = body.len() as u32;
        self.file.write_all(&len.to_le_bytes())?;
        self.file.write_all(&body)?;
        self.file.write_all(&crc.to_le_bytes())?;
        self.len += 4 + body.len() as u64 + 4;
        Ok(())
    }

    /// Flushes buffered appends to durable storage and advances the sidecar's last-fsync offset
    /// to match. Returns the new durable offset. The ack contract must not return 200 until this
    /// has returned `Ok`.
    pub fn fsync(&mut self) -> Result<u64> {
        self.file.sync_data()?;
        let mut sidecar = OpenOptions::new()
            .create(true)
            .write(true)
            .truncate(true)
            .open(&self.sync_path)?;
        sidecar.write_all(&self.len.to_le_bytes())?;
        sidecar.sync_all()?;
        Ok(self.len)
    }
}
