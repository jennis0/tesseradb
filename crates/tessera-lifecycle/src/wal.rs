//! The write-ahead log (lifecycle design §4).
//!
//! Ack contract (wired in Task 13): **WAL append → fsync → in-memory apply/swap → 200.** Never
//! ack before fsync — everything below exists to make that fsync boundary the one place acked
//! state can be trusted from, and everything past it disposable.
//!
//! On-disk layout: a fixed 6-byte header (`b"TWAL"` ‖ `u16 LE` format version), then a sequence
//! of framed records: `u32 LE len ‖ postcard bytes ‖ u32 LE crc32(postcard bytes)`.
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
//! last-fsynced offset, tracked in the sidecar file `<name>.sync` (8-byte LE offset, written via
//! write-tmp-then-rename and fsynced — together with its directory entry — immediately after
//! every WAL fsync; see [`Wal::fsync`]):
//!
//! - failing record starts **at or past** the sync offset → it was never fsynced, so it can only
//!   be an artefact of a torn tail write. Truncate the log there and replay succeeds with the
//!   records collected so far.
//! - failing record starts **before** the sync offset → it lies inside bytes that were reported
//!   durable. Fail closed: return [`WalError::WalCorruption`] and the caller must not become
//!   ready.
//!
//! Two failure modes that are easy to get fail-open by accident, and are guarded explicitly
//! here: a WAL file that is simply **shorter** than the recorded sync offset (no corrupted
//! record at all — the tail is just *gone*, e.g. a restored stale copy or lost filesystem
//! blocks) must fail closed exactly as a corrupted record before the sync point would; and a
//! **missing or unreadable sidecar** must default to "assume everything present is acked", not
//! "assume nothing is acked" — the latter would silently downgrade real, previously-fsynced
//! records to discardable tail noise the moment the sidecar is lost.
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
///
/// On-disk format: variant order is frozen and append-only (postcard encodes enum variants by
/// declaration index) — never reorder or remove a variant, only append new ones at the end.
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
///
/// `external_id` is **optional** (contracts §3.4 r6): a caller may ingest an item with no
/// external id at all, in which case it gets no sidecar entry and is addressable only by its
/// `tessera_id` — a pure function of `(key, shard_id, entity_id)`, so nothing needs to be stored
/// to make that identity durable. `None` here must never collide with `None` elsewhere, and must
/// never be treated as "an external id happens to be empty".
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct WalRow {
    pub external_id: Option<Vec<u8>>,
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
///
/// On-disk format: variant order is frozen and append-only — see [`WalScalar`]'s note.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum ChangeOp {
    Predicate,
    Delete,
    Suppress,
    Unsuppress,
}

/// One framed WAL record.
///
/// On-disk format: variant order is frozen and append-only — see [`WalScalar`]'s note.
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

/// WAL-level failures. [`WalError::WalCorruption`], [`WalError::BadHeader`] and
/// [`WalError::Poisoned`] are all fail-closed cases: the caller must not treat the WAL as
/// open/ready, and must not attempt further operations on a poisoned handle.
#[derive(Debug)]
pub enum WalError {
    Io(std::io::Error),
    Postcard(postcard::Error),
    /// Framing/CRC failure starting before the last-fsynced offset, or a WAL file shorter than
    /// that offset — acked state, possibly a deny, is damaged or missing. Fail closed (lifecycle
    /// design §4): do not come ready.
    WalCorruption,
    /// The file's leading magic/version header is missing or does not match. Not itself a
    /// framing/CRC failure, but the same fail-closed answer applies: a file we cannot positively
    /// identify as this WAL format must not be trusted or written to.
    BadHeader,
    /// A previous `append` or `fsync` failed partway through a write. `self.len` can no longer
    /// be trusted to name a record boundary in the underlying file, so every subsequent
    /// operation on this handle refuses rather than risk writing past a torn frame.
    Poisoned,
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
            WalError::BadHeader => write!(f, "wal file header missing or unrecognised"),
            WalError::Poisoned => write!(
                f,
                "wal handle poisoned by a previous write failure — reopen from disk"
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

/// File format magic, checked at open.
const WAL_MAGIC: [u8; 4] = *b"TWAL";
/// File format version, checked at open. Bump on any incompatible change to record framing or
/// the header itself.
const WAL_VERSION: u16 = 1;
/// Header size in bytes (`WAL_MAGIC` ‖ `WAL_VERSION` LE). Every record offset in this module —
/// including the ones compared against the sidecar's last-fsync offset — is a byte offset from
/// the start of the file, so it already accounts for the header living at the front.
pub const HEADER_LEN: u64 = WAL_MAGIC.len() as u64 + 2;

/// An open write-ahead log. `open` replays existing records; `append` buffers a new one;
/// `fsync` is the durability boundary the ack contract waits on.
pub struct Wal {
    file: File,
    sync_path: PathBuf,
    /// Current end-of-file offset — bytes appended so far (including the header), whether or
    /// not yet fsynced.
    len: u64,
    /// Set on any I/O error part-way through a write. Once poisoned, every further `append`/
    /// `fsync` call refuses immediately (I3) rather than risk `len` disagreeing with the file.
    poisoned: bool,
}

/// The sidecar lives beside the WAL file, named after its stem: `wal.log` → `wal.sync`. Not a
/// fixed `wal.sync` in the directory — a directory could plausibly host more than one WAL in
/// future, and naming it after the file it belongs to avoids collision.
fn sync_sidecar_path(wal_path: &Path) -> PathBuf {
    let dir = wal_path
        .parent()
        .filter(|d| !d.as_os_str().is_empty())
        .unwrap_or_else(|| Path::new("."));
    let stem = wal_path.file_stem().unwrap_or(wal_path.as_os_str());
    let mut name = stem.to_os_string();
    name.push(".sync");
    dir.join(name)
}

/// A `.tmp` sibling of `path`, used for the write-tmp-then-rename sidecar update (C3): a rename
/// is atomic, so there is never a window where the sidecar is truncated-but-not-yet-rewritten.
fn tmp_sibling(path: &Path) -> PathBuf {
    let mut os = path.as_os_str().to_os_string();
    os.push(".tmp");
    PathBuf::from(os)
}

/// Fsyncs a directory so that entries created or renamed within it (a new WAL file, a renamed
/// sidecar) are durable, not just the file contents themselves.
fn fsync_dir(dir: &Path) -> std::io::Result<()> {
    let dir = if dir.as_os_str().is_empty() {
        Path::new(".")
    } else {
        dir
    };
    File::open(dir)?.sync_all()
}

/// Resolves the last-fsync offset to replay against.
///
/// `wal_len` is the WAL file's byte length (including header) *before* this open's replay has
/// touched it. If the file carries no records yet (`wal_len <= HEADER_LEN`), there is nothing to
/// protect and the sync point is simply the file's current length. Otherwise:
///
/// - a well-formed 8-byte sidecar is trusted as-is;
/// - a missing or malformed sidecar defaults to `wal_len` — i.e. **everything present is assumed
///   acked** (fail closed: a lost/short sidecar must make corruption anywhere in a non-empty WAL
///   refuse to open, not silently look like an untouched tail — C2).
fn resolve_sync_point(sync_path: &Path, wal_len: u64) -> Result<u64> {
    if wal_len <= HEADER_LEN {
        return Ok(wal_len);
    }
    match std::fs::read(sync_path) {
        Ok(bytes) if bytes.len() == 8 => {
            let mut buf = [0u8; 8];
            buf.copy_from_slice(&bytes);
            Ok(u64::from_le_bytes(buf))
        }
        Ok(_) => Ok(wal_len),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(wal_len),
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

fn write_header(file: &mut File) -> std::io::Result<()> {
    file.seek(SeekFrom::Start(0))?;
    file.write_all(&WAL_MAGIC)?;
    file.write_all(&WAL_VERSION.to_le_bytes())?;
    file.sync_all()?;
    file.seek(SeekFrom::Start(HEADER_LEN))?;
    Ok(())
}

fn check_header(file: &mut File) -> Result<()> {
    file.seek(SeekFrom::Start(0))?;
    let mut buf = [0u8; HEADER_LEN as usize];
    let n = read_up_to(file, &mut buf)?;
    if n < HEADER_LEN as usize
        || buf[0..4] != WAL_MAGIC
        || u16::from_le_bytes([buf[4], buf[5]]) != WAL_VERSION
    {
        return Err(WalError::BadHeader);
    }
    Ok(())
}

/// Replays every record from just past the header, applying the positional CRC rule on the
/// first framing or CRC failure encountered. Returns the records collected and the offset
/// replay stopped at (== the file's logical length after any truncation).
fn replay(file: &mut File, sync_point: u64) -> Result<(Vec<WalRecord>, u64)> {
    let total_len = file.metadata()?.len();
    file.seek(SeekFrom::Start(HEADER_LEN))?;
    let mut records = Vec::new();
    let mut pos: u64 = HEADER_LEN;

    loop {
        let mut len_buf = [0u8; 4];
        let n = read_up_to(file, &mut len_buf)?;
        if n == 0 {
            // Clean end of log: nothing was ever written from `pos` onward. This is only a safe,
            // ackable state if `pos` has actually reached the last-fsynced offset (C1) — a WAL
            // that is simply *shorter* than what the sidecar claims was durable is exactly as
            // dangerous as a corrupted record before that offset, and must fail the same way.
            if pos < sync_point {
                return Err(WalError::WalCorruption);
            }
            break;
        }
        if n < 4 {
            return finish_on_failure(file, pos, sync_point, records);
        }
        let body_len = u32::from_le_bytes(len_buf) as u64;

        // Bound the claimed body length against what the file can actually hold before
        // allocating for it: a corrupted length prefix (e.g. a stray 0xFFFFFFFF) must be treated
        // as a framing failure at `pos`, not turned into a multi-gigabyte allocation attempt
        // (I1). (The short-read check below would eventually catch an over-long body too, but
        // only after the allocation already happened — this bound avoids paying for it at all.)
        let remaining = total_len.saturating_sub(pos + 4);
        if body_len > remaining {
            return finish_on_failure(file, pos, sync_point, records);
        }

        let mut body = vec![0u8; body_len as usize];
        if (read_up_to(file, &mut body)? as u64) < body_len {
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

        // Framing + CRC alone cannot catch every corruption: an all-zero region (e.g. sparse-file
        // zero-fill, or a hole left by a crash mid-write with no CRC ever written) has
        // `body_len == 0` and `crc32fast::hash(&[]) == 0`, which passes both checks trivially.
        // `postcard::from_bytes` is the backstop here — an empty (or otherwise all-zero) byte
        // string cannot select any `WalRecord` variant, so decode fails and this record is
        // still routed through `finish_on_failure` like any other corrupt record.
        let record: WalRecord = match postcard::from_bytes(&body) {
            Ok(r) => r,
            Err(_) => return finish_on_failure(file, pos, sync_point, records),
        };

        records.push(record);
        pos += 4 + body_len + 4;
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
        // At or past the sync point: never acked. Safe to discard silently — but fsync the
        // truncation immediately (I2), so a second crash before the next explicit `Wal::fsync`
        // cannot let the filesystem resurrect the stale tail we just decided to drop.
        file.set_len(pos)?;
        file.sync_data()?;
        Ok((records, pos))
    }
}

impl Wal {
    /// Opens (creating if absent) the WAL at `path`, replays it under the positional CRC rule,
    /// and returns the live handle plus every record recovered.
    pub fn open<P: AsRef<Path>>(path: P) -> Result<(Wal, Vec<WalRecord>)> {
        let path = path.as_ref();
        let sync_path = sync_sidecar_path(path);

        let is_new = !path.exists();

        let mut file = OpenOptions::new()
            .create(true)
            .truncate(false)
            .read(true)
            .write(true)
            .open(path)?;

        let initial_len = file.metadata()?.len();
        if initial_len == 0 {
            write_header(&mut file)?;
        } else {
            check_header(&mut file)?;
        }
        if is_new {
            // The directory entry for a brand-new WAL file must itself be durable (C3) —
            // otherwise a crash immediately after creation can lose the file entirely while its
            // sidecar (if any survives from a same-named predecessor) still claims a sync point.
            if let Some(dir) = path.parent().filter(|d| !d.as_os_str().is_empty()) {
                fsync_dir(dir)?;
            } else {
                fsync_dir(Path::new("."))?;
            }
        }

        let wal_len = file.metadata()?.len();
        let sync_point = resolve_sync_point(&sync_path, wal_len)?;

        let (records, len) = replay(&mut file, sync_point)?;
        file.seek(SeekFrom::Start(len))?;

        Ok((
            Wal {
                file,
                sync_path,
                len,
                poisoned: false,
            },
            records,
        ))
    }

    /// Buffers `rec` for append. Not durable until [`Wal::fsync`] returns.
    pub fn append(&mut self, rec: &WalRecord) -> Result<()> {
        if self.poisoned {
            return Err(WalError::Poisoned);
        }
        let body = postcard::to_allocvec(rec)?;
        let crc = crc32fast::hash(&body);
        let len = body.len() as u32;

        let write_result: std::io::Result<()> = (|| {
            self.file.write_all(&len.to_le_bytes())?;
            self.file.write_all(&body)?;
            self.file.write_all(&crc.to_le_bytes())?;
            Ok(())
        })();

        match write_result {
            Ok(()) => {
                self.len += 4 + body.len() as u64 + 4;
                Ok(())
            }
            Err(e) => {
                // A partially-completed write_all may have left the file at an offset between
                // the old and new `self.len` — we cannot know how many bytes actually landed, so
                // `self.len` can no longer be trusted to name a record boundary. Poison rather
                // than guess (I3).
                self.poisoned = true;
                Err(WalError::Io(e))
            }
        }
    }

    /// Whether a previous write failed part-way through, leaving `self.len` unable to name a
    /// record boundary. Every subsequent `append`/`fsync` on this handle refuses with
    /// [`WalError::Poisoned`].
    ///
    /// **This is the source of truth for the executor's not-ready posture** (lifecycle §4, plan
    /// Task 3a). The executor mirrors *this* rather than remembering that it once saw an `Err`,
    /// because a posture derived from the executor's own bookkeeping is a posture that can drift
    /// from the thing it claims to describe. Poisoning is set in exactly three places — `append`'s
    /// error arm and `fsync`'s two — and this accessor is how anything outside this module learns
    /// about it, since the field is private and must stay so.
    pub fn is_poisoned(&self) -> bool {
        self.poisoned
    }

    /// Flushes buffered appends to durable storage and advances the sidecar's last-fsync offset
    /// to match. Returns the new durable offset. The ack contract must not return 200 until this
    /// has returned `Ok`.
    pub fn fsync(&mut self) -> Result<u64> {
        if self.poisoned {
            return Err(WalError::Poisoned);
        }

        if let Err(e) = self.file.sync_data() {
            self.poisoned = true;
            return Err(WalError::Io(e));
        }

        let result: std::io::Result<()> = (|| {
            let tmp_path = tmp_sibling(&self.sync_path);
            {
                let mut tmp = OpenOptions::new()
                    .create(true)
                    .write(true)
                    .truncate(true)
                    .open(&tmp_path)?;
                tmp.write_all(&self.len.to_le_bytes())?;
                tmp.sync_all()?;
            }
            std::fs::rename(&tmp_path, &self.sync_path)?;
            let dir = self
                .sync_path
                .parent()
                .filter(|d| !d.as_os_str().is_empty())
                .unwrap_or_else(|| Path::new("."));
            fsync_dir(dir)?;
            Ok(())
        })();

        match result {
            Ok(()) => Ok(self.len),
            Err(e) => {
                self.poisoned = true;
                Err(WalError::Io(e))
            }
        }
    }
}

/// The write executor's **sole** WAL handle: a [`Wal`] owned by value, plus the counters and — in
/// test builds — the fault switches that make the ack contract observable (Phase 2 stage 2.1,
/// Task 3a).
///
/// ## Why the executor holds this rather than a bare `Wal`
///
/// Two things need to hang off every append and fsync, and neither belongs inside [`Wal`]:
///
/// - **The counters** ([`WalMeter`]), which are production telemetry. Task 7a's group commit is
///   *defined* by "one fsync per window" and is measured in exactly this number; the ingest
///   baseline memo's ~3.2 ms floor is a cost per unit of it. `Wal` should stay a file format and a
///   positional CRC rule, so the counting lives one layer out.
/// - **The fault switches**, which must not exist in a shipped binary at all (see
///   [`crate::faults`]). Putting a `#[cfg]` inside the durability primitive would mean the type
///   the whole fail-closed story rests on compiles differently in test and in production. Wrapping
///   it means `Wal` is byte-for-byte the same type either way.
///
/// ## Poisoning is mirrored, never remembered
///
/// [`Self::is_poisoned`] is `self.wal.is_poisoned() || self.injected_poison` — asked of the WAL on
/// every call, never cached from the last error the executor happened to see. A posture derived
/// from the executor's own bookkeeping can drift from the thing it claims to describe; one derived
/// from the WAL cannot. This is the value stage 2.1's not-ready posture is built on (lifecycle §4).
///
/// An **injected** failure follows the real sequence exactly — `Io` on the failing call, `Poisoned`
/// on every call after it — for the reason argued at length in [`crate::faults`]: the first call's
/// variant is the one the 500 mapping, the operator alarm and the deny apply-anyway branch all
/// switch on, so a harness that got it wrong would be testing itself.
pub struct ExecutorWal {
    wal: Wal,
    meter: std::sync::Arc<crate::faults::WalMeter>,
    /// Set by an injected failure, so injection poisons the *handle* exactly as a real I/O error
    /// poisons the `Wal` — otherwise `a_poisoned_wal_trips_the_not_ready_posture` would be
    /// asserting a property of the switchboard.
    #[cfg(feature = "fault-injection")]
    injected_poison: bool,
    #[cfg(feature = "fault-injection")]
    faults: Option<std::sync::Arc<crate::faults::FaultSwitchboard>>,
}

impl ExecutorWal {
    /// Take ownership of `wal`. There is exactly one of these per partition and it lives on the
    /// executor thread — that single ownership *is* the ordering guarantee stage 2.1 delivers, in
    /// place of Phase 1's `Mutex<Wal>` plus a fourteen-line comment explaining that holding it
    /// across append→fsync→apply→swap was load-bearing.
    pub fn new(wal: Wal, meter: std::sync::Arc<crate::faults::WalMeter>) -> Self {
        ExecutorWal {
            wal,
            meter,
            #[cfg(feature = "fault-injection")]
            injected_poison: false,
            #[cfg(feature = "fault-injection")]
            faults: None,
        }
    }

    /// Arm this handle with a fault switchboard. Test builds only.
    #[cfg(feature = "fault-injection")]
    pub fn with_faults(mut self, faults: std::sync::Arc<crate::faults::FaultSwitchboard>) -> Self {
        self.faults = Some(faults);
        self
    }

    /// Whether this handle refuses every further operation — the WAL's own poison, or an injected
    /// one. See the type doc: mirrored on every call, never cached.
    pub fn is_poisoned(&self) -> bool {
        #[cfg(feature = "fault-injection")]
        {
            self.injected_poison || self.wal.is_poisoned()
        }
        #[cfg(not(feature = "fault-injection"))]
        {
            self.wal.is_poisoned()
        }
    }

    pub fn append(&mut self, rec: &WalRecord) -> Result<()> {
        #[cfg(feature = "fault-injection")]
        if let Some(injected) = self.injected_failure(|f| f.take_append_failure()) {
            return Err(injected);
        }
        let out = self.wal.append(rec);
        if out.is_ok() {
            self.meter.record_append();
            #[cfg(feature = "fault-injection")]
            if let Some(faults) = &self.faults {
                faults.record(crate::faults::Step::Append);
            }
        }
        out
    }

    pub fn fsync(&mut self) -> Result<u64> {
        #[cfg(feature = "fault-injection")]
        if let Some(injected) = self.injected_failure(|f| f.take_fsync_failure()) {
            return Err(injected);
        }
        let out = self.wal.fsync();
        if out.is_ok() {
            self.meter.record_fsync();
            #[cfg(feature = "fault-injection")]
            if let Some(faults) = &self.faults {
                faults.record(crate::faults::Step::Fsync);
            }
        }
        out
    }

    /// `Some(err)` when this call must fail: either the handle is already poisoned (real or
    /// injected), or `take` armed a fresh failure. The two cases return **different** variants, and
    /// that is the whole fidelity rule — see [`crate::faults`].
    #[cfg(feature = "fault-injection")]
    fn injected_failure(
        &mut self,
        take: impl Fn(&crate::faults::FaultSwitchboard) -> bool,
    ) -> Option<WalError> {
        if self.injected_poison {
            return Some(WalError::Poisoned);
        }
        let faults = self.faults.as_ref()?;
        if !take(faults) {
            return None;
        }
        self.injected_poison = true;
        Some(WalError::Io(std::io::Error::new(
            std::io::ErrorKind::StorageFull,
            "injected disk-full (fault-injection build only)",
        )))
    }
}
