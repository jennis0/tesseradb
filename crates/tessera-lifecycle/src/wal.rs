//! The write-ahead log (lifecycle design §4).
//!
//! Ack contract (wired in Task 13): **WAL append → fsync → in-memory apply/swap → 200.** Never
//! ack before fsync — everything below exists to make that fsync boundary the one place acked
//! state can be trusted from, and everything past it disposable.
//!
//! On-disk layout: a fixed 6-byte header (`b"TWAL"` ‖ `u16 LE` format version), then a sequence
//! of framed records: `u32 LE len ‖ postcard bytes ‖ u32 LE crc32(postcard bytes)`.
//!
//! ## The durable prefix, and the positional rule over it
//!
//! **Replay reads the durable prefix of the log and nothing else.** The prefix ends at the
//! last-fsynced offset, tracked in the sidecar file `<name>.sync` (8-byte LE offset, written via
//! write-tmp-then-rename and fsynced — together with its directory entry — immediately after every
//! WAL fsync; see [`Wal::fsync`]). Everything past that offset is discarded, and the log is
//! truncated to it.
//!
//! The discard is not a tolerance for damage; it is what the acknowledgement contract requires in
//! both directions. Bytes past the last fsync were never acknowledged, because the ack path fsyncs
//! first — so a caller either received an error saying the write was not durable, or received
//! nothing at all and will retry. **Reinstating such a record would make an effect durable after
//! its caller was told it was not**, which is the same class of divergence as acknowledging one
//! that is not durable, and dangerous in the same way: ingest reappears that a client believes was
//! rejected, and a client that retried under a fresh batch identifier now holds two copies. Whether
//! those bytes happen to frame and checksum correctly is not evidence of anything — a full record
//! and a torn one are equally unacknowledged, so they get the same answer.
//!
//! Inside the prefix the rule inverts. A framing or CRC failure below the last-fsynced offset means
//! bytes a caller was told were durable — possibly a `delete` or `suppress` — are damaged. There is
//! no safe repair (skipping the record would silently un-deny something), so replay returns
//! [`WalError::WalCorruption`] and the caller must not become ready. Truncate-at-first-bad-CRC
//! applied mid-log would silently drop acked denies; the position is the whole difference between
//! crash recovery and data loss.
//!
//! A record that *straddles* the boundary — starting inside the prefix and ending past it — means
//! the sidecar names an offset that is not a record boundary, so the two disagree about what was
//! made durable. That is a statement about acked bytes and fails closed like any other.
//!
//! Three further modes are easy to get fail-open by accident and are guarded explicitly. A WAL file
//! **shorter** than the recorded sync offset (no corrupted record at all — the tail is simply gone,
//! e.g. a restored stale copy or lost filesystem blocks) fails closed exactly as damage inside the
//! prefix does. A **missing or unreadable sidecar** defaults to "everything present is acked"
//! rather than "nothing is acked"; the latter would silently downgrade real, previously-fsynced
//! records to discardable tail noise the moment the sidecar is lost. And the truncation itself is
//! fsynced before `open` returns, so a second crash cannot resurrect the tail that was just
//! discarded; **if the truncation fails, `open` fails**, and a handle that cannot establish where
//! its log ends is never handed out.
//!
//! ## Two kinds of write failure, and why only one of them is terminal
//!
//! A failed [`Wal::append`] and a failed [`Wal::fsync`] leave the handle in genuinely different
//! states, and treating them alike costs a durability that was still reachable.
//!
//! `append` writes with `write_all`, which may complete partially. After a failure an unknown
//! number of bytes landed, so `self.len` no longer names a record boundary and there is nothing
//! honest to do with the file but stop touching it. That is [`WalState::Torn`], and it is terminal.
//!
//! `fsync` writes nothing. `sync_data` failing leaves the file offset where it was and `self.len`
//! exact; the sidecar publish failing leaves the bytes durable and only the boundary unrecorded.
//! Both are states a handle can be brought *out* of, and [`Wal::retry_durability`] is how — see its
//! doc for the two repairs and for why one of them must re-write bytes rather than merely re-sync.
//! A handle in either state still refuses `append` and `fsync` and still reports
//! [`Wal::is_poisoned`], because until the repair succeeds nothing above the last durable offset
//! may be treated as written.
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
/// §3) are distinct and must not be conflated: deletion denies retire by the stamp ledger,
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
    /// Anything wrong inside the durable prefix: a framing or CRC failure below the last-fsynced
    /// offset, a WAL file shorter than that offset, or a record straddling it. In every case state
    /// a caller was told was durable — possibly a deny — is damaged, missing, or not where the
    /// sidecar says it is. Fail closed (lifecycle §4): do not come ready.
    WalCorruption,
    /// The file's leading magic/version header is missing or does not match. Not itself a
    /// framing/CRC failure, but the same fail-closed answer applies: a file we cannot positively
    /// identify as this WAL format must not be trusted or written to.
    BadHeader,
    /// A previous `append` or `fsync` failed, and the handle has not been repaired. Every
    /// `append`/`fsync` refuses: after a torn append `self.len` cannot name a record boundary, and
    /// after a failed sync nothing above the last durable offset may be built on. The two are
    /// distinguished by [`Wal::is_recoverable`], not by this variant — no caller of `append` or
    /// `fsync` has anything to do differently, and only [`Wal::retry_durability`] does.
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

/// What a handle is able to do next. See the module doc's account of why an append failure and a
/// sync failure are not the same event.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum WalState {
    /// Everything below `len` is written and everything below `durable_len` is durable.
    Healthy,
    /// `sync_data` failed. `len` is exact — `fsync` writes no bytes — but the bytes in
    /// `[durable_len, len)` may never have reached the device, and on Linux the kernel may have
    /// marked their pages clean while reporting the error exactly once. Repairable only by
    /// re-writing those bytes and syncing again.
    Unsynced,
    /// `sync_data` succeeded and the sidecar publish did not. The bytes are durable; the recorded
    /// boundary does not say so, so replay would discard them. Repairable by publishing alone.
    Unpublished,
    /// A `write_all` completed partially. `len` names no record boundary and no repair can
    /// establish one. Terminal.
    Torn,
}

/// An open write-ahead log. `open` replays existing records; `append` buffers a new one;
/// `fsync` is the durability boundary the ack contract waits on.
pub struct Wal {
    file: File,
    sync_path: PathBuf,
    /// Current end-of-file offset — bytes appended so far (including the header), whether or
    /// not yet fsynced.
    len: u64,
    /// The offset the sidecar names: everything below it is durable and acknowledged. Advanced
    /// only by a *complete* `sync_data` + publish, so `[durable_len, len)` is always exactly the
    /// region no caller has been told about — which is what makes it the region
    /// [`Wal::retry_durability`] may re-write.
    durable_len: u64,
    /// Set on any I/O error during a write or a sync. Every further `append`/`fsync` refuses
    /// immediately (I3) rather than risk `len` disagreeing with the file, or building on bytes
    /// that may not exist.
    state: WalState,
}

/// The on-disk size of a framed record whose postcard body is `body_len` bytes.
fn framed_len(body_len: usize) -> u64 {
    4 + body_len as u64 + 4
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

/// Publishes `offset` as the log's last-fsynced position, durably.
///
/// Write-tmp-then-rename (C3): a rename is atomic, so there is never a window in which the sidecar
/// is truncated-but-not-yet-rewritten and therefore reads as malformed. The directory entry is
/// fsynced too, or the rename itself could be lost while the bytes it names are not.
///
/// The caller is responsible for the ordering that makes the value true: the WAL bytes up to
/// `offset` must already be durable when this is called, never the other way round.
fn write_sync_offset(sync_path: &Path, offset: u64) -> std::io::Result<()> {
    let tmp_path = tmp_sibling(sync_path);
    {
        let mut tmp = OpenOptions::new()
            .create(true)
            .write(true)
            .truncate(true)
            .open(&tmp_path)?;
        tmp.write_all(&offset.to_le_bytes())?;
        tmp.sync_all()?;
    }
    std::fs::rename(&tmp_path, sync_path)?;
    let dir = sync_path
        .parent()
        .filter(|d| !d.as_os_str().is_empty())
        .unwrap_or_else(|| Path::new("."));
    fsync_dir(dir)
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

/// Replays the log's **durable prefix** — the records lying wholly below `sync_point` — and
/// discards whatever follows it. Returns the records collected and the offset replay stopped at,
/// which is the file's logical length once the tail has been truncated away.
///
/// Every failure inside the prefix is corruption of acknowledged state and returns
/// [`WalError::WalCorruption`]; see the module doc for why the answer is uniform here and uniform
/// the other way past the boundary.
fn replay(file: &mut File, sync_point: u64) -> Result<(Vec<WalRecord>, u64)> {
    let total_len = file.metadata()?.len();
    file.seek(SeekFrom::Start(HEADER_LEN))?;
    let mut records = Vec::new();
    let mut pos: u64 = HEADER_LEN;

    while pos < sync_point {
        let mut len_buf = [0u8; 4];
        if read_up_to(file, &mut len_buf)? < 4 {
            // End of file before the durable offset was reached: the log is *shorter* than what
            // the sidecar says was made durable (C1). No record is damaged, and that is exactly
            // what makes it dangerous — the acked bytes are simply gone.
            return Err(WalError::WalCorruption);
        }
        let body_len = u32::from_le_bytes(len_buf) as u64;

        // Bound the claimed frame against what the durable prefix can actually hold, before
        // allocating for it. Two things at once: a corrupted length prefix (e.g. a stray
        // 0xFFFFFFFF) never becomes a multi-gigabyte allocation attempt (I1), and a record that
        // would straddle the boundary — starting inside the prefix, ending past it — is refused
        // here rather than being read out of the undurable region. Both are failures below the
        // sync point, so both fail closed.
        let framed = 4 + body_len + 4;
        if framed > sync_point.saturating_sub(pos) {
            return Err(WalError::WalCorruption);
        }

        let mut body = vec![0u8; body_len as usize];
        if (read_up_to(file, &mut body)? as u64) < body_len {
            return Err(WalError::WalCorruption);
        }

        let mut crc_buf = [0u8; 4];
        if read_up_to(file, &mut crc_buf)? < 4 {
            return Err(WalError::WalCorruption);
        }
        if crc32fast::hash(&body) != u32::from_le_bytes(crc_buf) {
            return Err(WalError::WalCorruption);
        }

        // Framing + CRC alone cannot catch every corruption: an all-zero region (e.g. sparse-file
        // zero-fill, or a hole left by a crash mid-write with no CRC ever written) has
        // `body_len == 0` and `crc32fast::hash(&[]) == 0`, which passes both checks trivially.
        // `postcard::from_bytes` is the backstop — an empty (or otherwise all-zero) byte string
        // cannot select any `WalRecord` variant, so the decode fails and the record fails closed
        // like any other damaged one.
        let record: WalRecord = postcard::from_bytes(&body).map_err(|_| WalError::WalCorruption)?;

        records.push(record);
        pos += framed;
    }

    // Nothing past here was ever acknowledged. Discard it on disk rather than in memory alone, and
    // fsync the truncation before returning, so a crash before the next `Wal::fsync` cannot let the
    // filesystem resurrect the tail just dropped. A failure propagates: a handle that cannot
    // establish where its log ends must not be handed out.
    if total_len > pos {
        file.set_len(pos)?;
        file.sync_data()?;
    }

    Ok((records, pos))
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
            // **A log begins with a recorded durable boundary of "header only".** Without this, a
            // log that is created, appended to, and then denied its first fsync has no sidecar at
            // all, and the missing-sidecar default (everything present is acked — see the module
            // doc) would read back as acked exactly the records that failure told the caller it did
            // not have. That default is right for a sidecar that was *lost* and must not be reached
            // by a log that never had one. Any stale sidecar left by a same-named predecessor is
            // replaced here for the same reason.
            write_sync_offset(&sync_path, HEADER_LEN)?;
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
                // Replay ends exactly at the sync point (every record is bounded against it), and
                // the tail past it has just been truncated away, so the file's length *is* the
                // durable boundary at the moment a handle is issued.
                durable_len: len,
                state: WalState::Healthy,
            },
            records,
        ))
    }

    /// Buffers `rec` for append. Not durable until [`Wal::fsync`] returns.
    pub fn append(&mut self, rec: &WalRecord) -> Result<()> {
        if self.state != WalState::Healthy {
            return Err(WalError::Poisoned);
        }
        let body = postcard::to_allocvec(rec)?;
        match self.write_framed(&body) {
            Ok(()) => {
                self.len += framed_len(body.len());
                Ok(())
            }
            Err(e) => {
                // A partially-completed write_all may have left the file at an offset between
                // the old and new `self.len` — we cannot know how many bytes actually landed, so
                // `self.len` can no longer be trusted to name a record boundary. Poison rather
                // than guess (I3), and terminally: no repair can recover a boundary nothing
                // records.
                self.state = WalState::Torn;
                Err(WalError::Io(e))
            }
        }
    }

    /// `u32 LE len ‖ body ‖ u32 LE crc32(body)` at the file's current offset. The one place the
    /// frame is laid down, so [`Wal::append`] and [`Wal::retry_durability`] cannot come to disagree
    /// about what a record looks like.
    fn write_framed(&mut self, body: &[u8]) -> std::io::Result<()> {
        let crc = crc32fast::hash(body);
        self.file.write_all(&(body.len() as u32).to_le_bytes())?;
        self.file.write_all(body)?;
        self.file.write_all(&crc.to_le_bytes())?;
        Ok(())
    }

    /// Whether a previous write or sync failed and has not been repaired. Every subsequent
    /// `append`/`fsync` on this handle refuses with [`WalError::Poisoned`].
    ///
    /// **This is the source of truth for the executor's not-ready posture** (lifecycle §4). The
    /// executor mirrors *this* rather than remembering that it once saw an `Err`, because a posture
    /// derived from the executor's own bookkeeping is a posture that can drift from the thing it
    /// claims to describe — and it can drift in both directions now that a poison can be cleared:
    /// after a successful [`Wal::retry_durability`] nothing is owed, and a node that went on
    /// reporting itself unready would be as wrong as one that never reported it. This accessor is
    /// how anything outside this module learns the state, since the field is private and must stay
    /// so.
    pub fn is_poisoned(&self) -> bool {
        self.state != WalState::Healthy
    }

    /// Whether the poison came from a *sync* rather than from a torn write, so
    /// [`Wal::retry_durability`] has something to repair.
    ///
    /// Deliberately not consulted by `append` or `fsync`: a recoverable handle refuses both exactly
    /// as a torn one does, because until the repair succeeds nothing above the last durable offset
    /// may be built on. The distinction exists for the one caller that is about to attempt the
    /// repair, and for nobody else.
    pub fn is_recoverable(&self) -> bool {
        matches!(self.state, WalState::Unsynced | WalState::Unpublished)
    }

    /// Flushes buffered appends to durable storage and advances the sidecar's last-fsync offset
    /// to match. Returns the new durable offset. The ack contract must not return 200 until this
    /// has returned `Ok`.
    pub fn fsync(&mut self) -> Result<u64> {
        if self.state != WalState::Healthy {
            return Err(WalError::Poisoned);
        }
        self.sync_and_publish()
    }

    /// Try again to make `[durable_len, len)` durable, given the records that region holds.
    ///
    /// ## Why a second `fsync` is not the repair
    ///
    /// On Linux a writeback error may be reported **once**: the kernel can mark the failed dirty
    /// page clean and clear the error on the next `fsync`, so a bare re-sync returns success with
    /// the data gone. (This is the behaviour that produced PostgreSQL's 2018 fsync reckoning; it is
    /// not a hypothetical.) A repair therefore has to make the kernel dirty those pages again, and
    /// the only way to do that is to write the bytes again.
    ///
    /// So [`WalState::Unsynced`] **rewinds to `durable_len` and re-writes the records in place**,
    /// then syncs. Rewinding rather than appending a second copy is what makes this a repair rather
    /// than a duplication: the region being overwritten is by construction the region no caller was
    /// ever told about — `durable_len` advances only on a complete sync-and-publish — so re-writing
    /// it is indistinguishable, to every reader, from the first write having succeeded. Appending a
    /// second copy would also be *safe* (a disposition is idempotent, and replay folds two identical
    /// `Change` records to the same overlay), but it would leave the possibly-lost first copy inside
    /// the durable prefix, where a hole reads as corruption and refuses the whole log at open.
    ///
    /// [`WalState::Unpublished`] needs no re-write at all: `sync_data` already returned, so the
    /// bytes are durable and only the boundary record is missing. It publishes alone.
    ///
    /// `records` must be exactly the records appended since the last successful sync, in order.
    /// That is checked, not trusted: their framed length must reproduce `len` exactly, or the call
    /// refuses without touching the file. Postcard encoding is deterministic, so re-encoding a
    /// record reproduces its bytes; the length equality is what pins that the *set* is right.
    ///
    /// A healthy handle is accepted and simply syncs, which is what this reduces to when nothing
    /// has been lost.
    pub fn retry_durability(&mut self, records: &[WalRecord]) -> Result<u64> {
        if self.state == WalState::Torn {
            return Err(WalError::Poisoned);
        }

        // Checked in **every** state, including the two that will not re-write a byte. The check
        // costs one re-encode of a window on a path that has already failed, and it is the only
        // thing standing between "the caller says these are the undurable records" and a repair
        // that acts on it — so making it conditional on the branch that happens to need it would
        // leave the other branches silently accepting a claim they never test.
        let mut bodies = Vec::with_capacity(records.len());
        let mut total = 0u64;
        for rec in records {
            let body = postcard::to_allocvec(rec)?;
            total += framed_len(body.len());
            bodies.push(body);
        }
        if self.durable_len + total != self.len {
            // Refuse rather than write a region whose extent we cannot predict — and leave the
            // state alone, so the handle goes on refusing everything.
            return Err(WalError::Poisoned);
        }

        if self.state == WalState::Unsynced {
            let rewrite: std::io::Result<()> = (|| {
                self.file.seek(SeekFrom::Start(self.durable_len))?;
                for body in &bodies {
                    self.write_framed(body)?;
                }
                Ok(())
            })();
            if let Err(e) = rewrite {
                // Same reasoning as `append`'s error arm, and worse: the file offset is now
                // somewhere inside a region we were repairing. Terminal.
                self.state = WalState::Torn;
                return Err(WalError::Io(e));
            }
        }

        self.sync_and_publish()
    }

    /// `sync_data` then publish the boundary, recording which half failed.
    ///
    /// The two arms are separate states rather than one poison because they are separately
    /// repairable — see [`Wal::retry_durability`].
    fn sync_and_publish(&mut self) -> Result<u64> {
        if let Err(e) = self.file.sync_data() {
            self.state = WalState::Unsynced;
            return Err(WalError::Io(e));
        }

        match write_sync_offset(&self.sync_path, self.len) {
            Ok(()) => {
                self.durable_len = self.len;
                self.state = WalState::Healthy;
                Ok(self.len)
            }
            Err(e) => {
                self.state = WalState::Unpublished;
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
///
/// **Recoverability is part of that fidelity.** A real append failure is terminal and a real sync
/// failure is not ([`WalState`]), so an injected append failure poisons terminally and an injected
/// sync failure poisons recoverably. Getting this wrong in the safe-looking direction — every
/// injected failure terminal — would make the deny lane's retry untestable while looking correct.
pub struct ExecutorWal {
    wal: Wal,
    meter: std::sync::Arc<crate::faults::WalMeter>,
    /// Set by an injected failure, so injection poisons the *handle* exactly as a real I/O error
    /// poisons the `Wal` — otherwise `a_poisoned_wal_trips_the_not_ready_posture` would be
    /// asserting a property of the switchboard.
    #[cfg(feature = "fault-injection")]
    injected: Option<InjectedPoison>,
    #[cfg(feature = "fault-injection")]
    faults: Option<std::sync::Arc<crate::faults::FaultSwitchboard>>,
}

/// Which of [`WalState`]'s two poisons an injected failure stands in for.
#[cfg(feature = "fault-injection")]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum InjectedPoison {
    /// Mirrors [`WalState::Torn`]: an injected append failure. No repair.
    Torn,
    /// Mirrors [`WalState::Unsynced`]/[`WalState::Unpublished`]: an injected sync failure.
    /// [`ExecutorWal::retry_durability`] clears it unless the switchboard has another failure armed.
    Recoverable,
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
            injected: None,
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
            self.injected.is_some() || self.wal.is_poisoned()
        }
        #[cfg(not(feature = "fault-injection"))]
        {
            self.wal.is_poisoned()
        }
    }

    /// Whether the poison is a sync failure with something for [`Self::retry_durability`] to
    /// repair — [`Wal::is_recoverable`], plus the injected mirror of it.
    pub fn is_recoverable(&self) -> bool {
        #[cfg(feature = "fault-injection")]
        {
            if self.injected.is_some() {
                return self.injected == Some(InjectedPoison::Recoverable);
            }
        }
        self.wal.is_recoverable()
    }

    pub fn append(&mut self, rec: &WalRecord) -> Result<()> {
        #[cfg(feature = "fault-injection")]
        if let Some(injected) =
            self.injected_failure(InjectedPoison::Torn, |f| f.take_append_failure())
        {
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
        if let Some(injected) =
            self.injected_failure(InjectedPoison::Recoverable, |f| f.take_fsync_failure())
        {
            return Err(injected);
        }
        let out = self.wal.fsync();
        self.count_sync(&out);
        out
    }

    /// Repair a sync failure — see [`Wal::retry_durability`], which this is the metered and
    /// fault-injectable face of.
    ///
    /// The injected path consults the switchboard's fsync arm again rather than clearing
    /// unconditionally, so a test can arm *n* failures and observe a retry sequence exhausting
    /// rather than only its first attempt.
    pub fn retry_durability(&mut self, records: &[WalRecord]) -> Result<u64> {
        #[cfg(feature = "fault-injection")]
        {
            match self.injected {
                Some(InjectedPoison::Torn) => return Err(WalError::Poisoned),
                Some(InjectedPoison::Recoverable) => {
                    if let Some(faults) = self.faults.as_ref() {
                        if faults.take_fsync_failure() {
                            return Err(WalError::Io(std::io::Error::new(
                                std::io::ErrorKind::StorageFull,
                                "injected disk-full (fault-injection build only)",
                            )));
                        }
                    }
                    // The underlying `Wal` never saw the injected failure, so its own bytes are
                    // merely un-synced rather than possibly-lost; the real sync below is the repair.
                    self.injected = None;
                }
                None => {}
            }
        }
        let out = self.wal.retry_durability(records);
        self.count_sync(&out);
        out
    }

    /// Count a successful sync, whichever call achieved it. A failed one is not counted: the meter
    /// measures durability actually achieved.
    fn count_sync(&self, out: &Result<u64>) {
        if out.is_ok() {
            self.meter.record_fsync();
            #[cfg(feature = "fault-injection")]
            if let Some(faults) = &self.faults {
                faults.record(crate::faults::Step::Fsync);
            }
        }
    }

    /// `Some(err)` when this call must fail: either the handle is already poisoned (real or
    /// injected), or `take` armed a fresh failure. The two cases return **different** variants, and
    /// that is the whole fidelity rule — see [`crate::faults`]. `kind` is the poison a fresh
    /// failure leaves behind, which differs by operation exactly as the real `Wal`'s does.
    #[cfg(feature = "fault-injection")]
    fn injected_failure(
        &mut self,
        kind: InjectedPoison,
        take: impl Fn(&crate::faults::FaultSwitchboard) -> bool,
    ) -> Option<WalError> {
        if self.injected.is_some() {
            return Some(WalError::Poisoned);
        }
        let faults = self.faults.as_ref()?;
        if !take(faults) {
            return None;
        }
        self.injected = Some(kind);
        Some(WalError::Io(std::io::Error::new(
            std::io::ErrorKind::StorageFull,
            "injected disk-full (fault-injection build only)",
        )))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn record(tag: u8) -> WalRecord {
        WalRecord::Change {
            external_id: vec![tag; 3],
            op: ChangeOp::Delete,
            descriptors: None,
        }
    }

    /// Overwrite `[from, to)` with zeroes through a second handle, standing in for pages a failed
    /// writeback left never written.
    fn blank_region(path: &Path, from: u64, to: u64) {
        let mut f = OpenOptions::new().write(true).open(path).unwrap();
        f.seek(SeekFrom::Start(from)).unwrap();
        f.write_all(&vec![0u8; (to - from) as usize]).unwrap();
        f.sync_data().unwrap();
    }

    /// **The repair re-writes bytes, and this is where "re-writes" is demonstrated rather than
    /// claimed.**
    ///
    /// The `sync_data` half of a durability failure cannot be provoked from a test in this tree —
    /// a read-only directory fails the sidecar publish, not the sync, which is why the
    /// integration-level `a_real_fsync_failure_is_repaired_by_retrying_durability` exercises the
    /// other half. So this one assembles the state directly and, more importantly, assembles the
    /// *damage*: the undurable region is blanked on disk, exactly as a dropped dirty page would
    /// leave it. A repair that only re-synced would leave the hole there.
    #[test]
    fn a_lost_writeback_is_repaired_by_rewriting_the_records() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("wal.log");
        {
            let (mut wal, _) = Wal::open(&path).unwrap();
            wal.append(&record(0)).unwrap();
            wal.fsync().unwrap();
            let durable = wal.durable_len;

            wal.append(&record(1)).unwrap();
            wal.append(&record(2)).unwrap();
            let end = wal.len;

            // The sync failed, and the pages it was meant to write are gone.
            wal.state = WalState::Unsynced;
            blank_region(&path, durable, end);

            wal.retry_durability(&[record(1), record(2)])
                .expect("the repair must succeed");
            assert!(!wal.is_poisoned());
        }

        let (_wal, records) = Wal::open(&path).unwrap();
        assert_eq!(
            records,
            vec![record(0), record(1), record(2)],
            "the re-written records must be back, in order and exactly once"
        );
    }

    /// **The naive repair — publish the boundary and hope — is unsafe, and this is what it costs.**
    ///
    /// It is the shape a reader will reach for, because on Linux a second `fsync` after a writeback
    /// error can return success with the data gone: the kernel may mark the failed page clean and
    /// report the error exactly once. Modelled here by taking the branch that publishes without
    /// re-writing over the same blanked region.
    ///
    /// The result is fail-*closed* rather than a leak — a zeroed frame inside the durable prefix
    /// cannot decode, so the log refuses to open — but a node that will not start is not a repair,
    /// and it is a strictly worse outcome than the un-repaired failure, which simply truncates the
    /// tail. This is the test that makes the rewrite load-bearing instead of decorative.
    #[test]
    fn publishing_a_lost_writeback_without_rewriting_it_fails_closed() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("wal.log");
        {
            let (mut wal, _) = Wal::open(&path).unwrap();
            wal.append(&record(0)).unwrap();
            wal.fsync().unwrap();
            let durable = wal.durable_len;

            wal.append(&record(1)).unwrap();
            let end = wal.len;

            // `Unpublished` is the state that skips the rewrite, so this is the bare re-sync.
            wal.state = WalState::Unpublished;
            blank_region(&path, durable, end);

            wal.retry_durability(&[record(1)])
                .expect("a bare re-sync reports success — that is the whole trap");
        }

        assert!(
            matches!(Wal::open(&path), Err(WalError::WalCorruption)),
            "a hole published inside the durable prefix must refuse to open"
        );
    }
}
