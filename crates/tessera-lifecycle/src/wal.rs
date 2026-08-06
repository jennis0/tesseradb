//! The write-ahead log (lifecycle design §4).
//!
//! Ack contract: **WAL append → fsync → in-memory apply/swap → 200.** Never
//! ack before fsync — everything below exists to make that fsync boundary the one place acked
//! state can be trusted from, and everything past it disposable.
//!
//! On-disk layout: a fixed 22-byte header (`b"TWAL"` ‖ `u16 LE` format version ‖ `u64 LE` member
//! number ‖ `u64 LE` base position), then framed records:
//! `u32 LE len ‖ postcard bytes ‖ u32 LE crc32(postcard bytes)`.
//!
//! ## The log is a sequence, not a file
//!
//! A caller names one path — `<dir>/wal.log` — and that names a **family**:
//! `<dir>/wal-000001.log`, `<dir>/wal-000002.log`, … each with its own fsync-offset sidecar
//! (decision 0038). The base path is never itself a file.
//!
//! It has to be a sequence because a flush makes a prefix of the log redundant and there is no way
//! to reclaim the front of a single file. [`Wal::rotate`] seals the active member, opens the next
//! one with an overlay snapshot at its head, and deletes whatever now lies wholly behind a flush's
//! `wal_pos`. Everything the single-file design guaranteed applies **per member, unchanged**: the
//! positional CRC rule, the three sidecar guards, truncate-and-fsync before a handle is issued.
//!
//! **Offsets are per file; positions are sequence-global.** A position counts record bytes across
//! every member the sequence has ever held, so it stays meaningful after the file it named has been
//! deleted — which is what lets a flush record a `wal_pos` and a later rotation act on it. Each
//! member's header carries the position it begins at, and `open` checks that every member continues
//! its predecessor's: a file that does not is stale or foreign, and every position derived from it
//! afterwards would name the wrong bytes.
//!
//! Two sequence-level failures fail closed, both for the same reason as the positional rule. A
//! **gap** in the numbering means a member was lost or deleted out of order, taking acked records
//! with it and leaving nothing to mark their absence — which is why reclamation deletes oldest
//! first. A **broken position chain** means a member has taken another's place in the walk.
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
/// Mirrors `tessera_spatial::tiler::ScalarValue`'s three kinds. **Duplicated deliberately** — a
/// reader would otherwise "fix" it: this crate's dependencies are `tessera-types`, `postcard` and
/// `crc32fast` alone (no `tessera-spatial`), so the WAL carries its own copy of the tiny, stable
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
///
/// `slice` names the row space the row's future row belongs to. It is durable rather than
/// re-derived because a flush segment covers a contiguous entity range only *within one slice*:
/// with more than one slice a commit window's entity range interleaves across them, and a
/// segment's range becomes ascending-with-holes. The row is the only place that fact survives a
/// restart, and the WAL is append-only — so the field goes in while the layout is still being
/// revised, not once a published segment depends on it. The handler resolves it against the
/// bundle's declared slices and refuses anything else; nothing defaults it, because a defaulted
/// slice is how a row silently joins the wrong row space.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct WalRow {
    pub external_id: Option<Vec<u8>>,
    pub entity_id: EntityId,
    pub slice: String,
    pub descriptors: Vec<Vec<u8>>,
    pub x: f32,
    pub y: f32,
    pub scalars: Vec<WalScalar>,
}

/// The disposition change carried by a [`WalRecord::ChangeByEntity`] record. The two removal rules
/// (write-path §5.4; ruled 2026-08-03) are distinct and must not be conflated: suppressions retire
/// only on `Unsuppress` (never touching postings — Rule S); deletions retire at the compaction fold
/// that executes them (Rule F).
///
/// A fourth variant, `Predicate`, was deleted with `WAL_VERSION` 5: decision 0047 withdrew the op
/// at the boundary (an edit is a delete plus a re-ingest) and decision 0048 deleted the machinery
/// that had been kept dormant for pre-0047 logs, there being none. A future predicate mechanism
/// would be designed, not resurrected.
///
/// On-disk format: variant order is frozen and append-only — see [`WalScalar`]'s note.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum ChangeOp {
    Delete,
    Suppress,
    Unsuppress,
}

/// One entity's disposition inside a [`WalRecord::OverlaySnapshot`].
///
/// Two shape decisions here are not free choices, and both are load-bearing for rotation.
///
/// **Keyed by [`EntityId`], never by external id.** An external-id-keyed snapshot would re-resolve
/// each id at replay, and an entity deleted before it was ever flushed has no row and may have no
/// extent entry — so replay could not resolve it and the node would refuse to open. A snapshot is
/// state that was already resolved once; resolving it again can only lose. It is the same reason
/// [`WalRecord::ChangeByEntity`] is keyed by entity, arrived at from the other end.
///
/// On-disk format: field order is positional under postcard — see [`WalRow`]'s note.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct OverlaySnapshotEntry {
    pub entity_id: EntityId,
    pub op: ChangeOp,
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
    /// The whole live overlay, written so that the change records it was accumulated from can be
    /// deleted.
    ///
    /// **The overlay's only durable home is the WAL.** Nothing else on disk carries a suppression:
    /// segments carry rows and postings, and the side-manifest carries geometry. So rotation cannot
    /// reclaim a WAL file until the dispositions inside it have been re-stated somewhere that
    /// survives — which is this record, and why ordering constraint 2 has the snapshot land before
    /// anything is deleted.
    ///
    /// It is a full snapshot, not a diff: the overlay is O(entities ever denied) — the quantity the
    /// `overlay_soft_limit` gauge already watches — which makes that gauge the right alarm for
    /// rotation cost too, and makes a snapshot self-sufficient rather than a link in a chain that
    /// fails closed only if every earlier link survives.
    ///
    /// Under replay it is an ordinary record in position: it is applied where it occurs, and a
    /// `ChangeByEntity` earlier in the same file still applies before it. See [`crate::replay`].
    OverlaySnapshot { entries: Vec<OverlaySnapshotEntry> },
    /// An accepted `/control/changes` entry, addressed by **entity id**.
    ///
    /// **Why the entity and not the identifier the caller supplied.** A `tessera_id` is a keyed
    /// permutation of entity space, so a record carrying one would resolve under whatever key the
    /// bundle holds at replay — a rotation would silently redirect every such deny to a different
    /// entity. Inverting once, at admission, and persisting the result is what makes replay
    /// identical across a rotation. It is the same reason [`OverlaySnapshotEntry`] is keyed by
    /// entity, arrived at from the other end.
    ///
    /// It also closes a hole an external-id-keyed record cannot: contracts §3.4 r6 makes an
    /// external id optional at ingest, and an item that arrived without one would be addressable by
    /// nothing — not deletable, not suppressible, at all. The external-id-keyed `Change` variant
    /// this one was added alongside was deleted with `WAL_VERSION` 5 (decision 0048), and replay
    /// stopped resolving external ids at all: the resolution now happens once, in the handler, at
    /// admission.
    ChangeByEntity { entity_id: EntityId, op: ChangeOp },
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
/// the header itself — and on any change to a record's *field* layout or the variant table, since
/// postcard encodes struct fields and enum discriminants positionally and would otherwise decode
/// a missing field or shifted variant as whatever bytes follow it. Version 2 added
/// [`WalRow::slice`]; version 3 made the log a sequence and put each member's number and base
/// position in its header; version 4 deleted the `Lease` and `Flush` variants — the first was
/// written by nothing (allocation rides `IngestBatch` rows), the second was written and read by
/// nothing (recovery reconstructs the buffer by the has-a-row predicate and rotation computes its
/// own reclaim bound), and deleting them shifts every later discriminant, which is exactly what
/// this version check exists to refuse. Version 5 deleted the `Change` variant, `ChangeOp`'s
/// `Predicate` and the `descriptors` field of `ChangeByEntity` and `OverlaySnapshotEntry`
/// (decision 0048): `Change` was written by nothing — every accepted change is admitted against an
/// entity — and the descriptors had no consumer once the evaluate store went. That shifts a variant
/// index, drops an enum discriminant and drops a struct field, each of which postcard would decode
/// as whatever bytes follow it.
const WAL_VERSION: u16 = 5;
/// Header size in bytes: `WAL_MAGIC` ‖ `WAL_VERSION` LE ‖ member number LE ‖ base position LE.
/// Every *offset* in this module is a byte offset from the start of its own file, so it already
/// accounts for the header living at the front; every *position* is sequence-global and counts
/// record bytes only — see [`Wal::position`].
pub const HEADER_LEN: u64 = WAL_MAGIC.len() as u64 + 2 + 8 + 8;

/// Digits in a member's zero-padded number. Fixed width so lexical order is numerical order, which
/// makes a directory listing already sorted oldest-first — the order reclamation must delete in.
const MEMBER_DIGITS: usize = 6;

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

/// The **sequence base**: the directory, stem and extension every member's name is derived from.
///
/// The caller still names a single path — `<dir>/wal.log` — and this turns it into the family
/// `<dir>/wal-000001.log`, `<dir>/wal-000002.log`, … The base path itself is never a file.
#[derive(Debug, Clone)]
struct SequenceBase {
    dir: PathBuf,
    stem: std::ffi::OsString,
    ext: Option<std::ffi::OsString>,
}

impl SequenceBase {
    fn of(path: &Path) -> Self {
        SequenceBase {
            dir: path
                .parent()
                .filter(|d| !d.as_os_str().is_empty())
                .unwrap_or_else(|| Path::new("."))
                .to_path_buf(),
            stem: path.file_stem().unwrap_or(path.as_os_str()).to_os_string(),
            ext: path.extension().map(|e| e.to_os_string()),
        }
    }

    /// `<dir>/<stem>-<n:06>.<ext>`.
    fn member(&self, n: u64) -> PathBuf {
        let mut name = self.stem.clone();
        name.push(format!(
            "-{n:0MEMBER_DIGITS$}",
            MEMBER_DIGITS = MEMBER_DIGITS
        ));
        if let Some(ext) = &self.ext {
            name.push(".");
            name.push(ext);
        }
        self.dir.join(name)
    }

    /// The sidecar lives beside its member and is named after it: `wal-000001.log` →
    /// `wal-000001.sync`. Per member, never one for the sequence — decision 0038, and the reason
    /// rotation can seal a file without any shared state having to be rewritten.
    fn sidecar(&self, n: u64) -> PathBuf {
        let mut name = self.stem.clone();
        name.push(format!(
            "-{n:0MEMBER_DIGITS$}.sync",
            MEMBER_DIGITS = MEMBER_DIGITS
        ));
        self.dir.join(name)
    }

    /// The member number `name` denotes, if it is one of this sequence's files.
    fn number_of(&self, name: &std::ffi::OsStr) -> Option<u64> {
        let name = name.to_str()?;
        let stem = self.stem.to_str()?;
        let rest = name.strip_prefix(stem)?.strip_prefix('-')?;
        let digits = match &self.ext {
            Some(ext) => rest.strip_suffix(&format!(".{}", ext.to_str()?))?,
            None => rest,
        };
        if digits.len() < MEMBER_DIGITS || !digits.bytes().all(|b| b.is_ascii_digit()) {
            return None;
        }
        digits.parse().ok()
    }

    /// Every member present on disk, ascending. Contiguity is checked by the caller, not here —
    /// a gap is a fail-closed condition, not an absence.
    fn members(&self) -> Result<Vec<u64>> {
        let mut found = Vec::new();
        match std::fs::read_dir(&self.dir) {
            Ok(entries) => {
                for entry in entries {
                    let entry = entry?;
                    if let Some(n) = self.number_of(&entry.file_name()) {
                        found.push(n);
                    }
                }
            }
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
            Err(e) => return Err(e.into()),
        }
        found.sort_unstable();
        Ok(found)
    }
}

/// One member file of the sequence, open for append.
struct WalFile {
    number: u64,
    sync_path: PathBuf,
    file: File,
    /// The sequence-global position of this file's **first record byte** — the total record bytes
    /// in every earlier member. Held in the header so the chain can be checked at open: a member
    /// whose base position does not continue its predecessor's is a stale or foreign file, not a
    /// continuation, and saying so is cheaper than discovering it through a `wal_pos` comparison
    /// that quietly means the wrong thing.
    base_pos: u64,
    /// Current end-of-file offset — bytes written so far, header included, fsynced or not.
    len: u64,
    /// The offset the sidecar names: everything below it is durable and acknowledged. Advanced
    /// only by a *complete* `sync_data` + publish, so `[durable_len, len)` is always exactly the
    /// region no caller has been told about — which is what makes it the region
    /// [`Wal::retry_durability`] may re-write.
    durable_len: u64,
}

impl WalFile {
    /// The sequence-global position just past this file's durable bytes.
    fn end_pos(&self) -> u64 {
        self.base_pos + (self.durable_len - HEADER_LEN)
    }
}

/// A member that is no longer appended to: its span, kept so reclamation can decide whether it
/// lies wholly below a `wal_pos` without reopening it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct SealedSpan {
    number: u64,
    end_pos: u64,
}

/// An open write-ahead log — a **sequence** of member files. `open` replays every surviving member
/// in order; `append` buffers a new record in the last of them; `fsync` is the durability boundary
/// the ack contract waits on; `rotate` seals the active member and reclaims what a flush has made
/// redundant.
pub struct Wal {
    base: SequenceBase,
    /// Members no longer appended to, ascending — the reclamation candidates.
    sealed: Vec<SealedSpan>,
    /// The member being appended to. Never a reclamation candidate.
    active: WalFile,
    /// Set on any I/O error during a write or a sync. Every further `append`/`fsync` refuses
    /// immediately (I3) rather than risk `len` disagreeing with the file, or building on bytes
    /// that may not exist.
    state: WalState,
    /// Where each record `open` replayed sits in the sequence — see [`Wal::replayed_positions`].
    replayed_positions: Vec<u64>,
}

/// The on-disk size of a framed record whose postcard body is `body_len` bytes.
fn framed_len(body_len: usize) -> u64 {
    4 + body_len as u64 + 4
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

fn write_header(file: &mut File, number: u64, base_pos: u64) -> std::io::Result<()> {
    file.seek(SeekFrom::Start(0))?;
    file.write_all(&WAL_MAGIC)?;
    file.write_all(&WAL_VERSION.to_le_bytes())?;
    file.write_all(&number.to_le_bytes())?;
    file.write_all(&base_pos.to_le_bytes())?;
    file.sync_all()?;
    file.seek(SeekFrom::Start(HEADER_LEN))?;
    Ok(())
}

/// The `(number, base_pos)` a member's header claims, refusing anything this format cannot
/// positively identify.
///
/// The number is checked against the *filename* here rather than merely read: a member renamed or
/// copied into the sequence would otherwise take its predecessor's place in the walk, and every
/// position derived afterwards would be silently wrong. A file we cannot identify is not trusted
/// and not written to.
fn check_header(file: &mut File, expected_number: u64) -> Result<(u64, u64)> {
    file.seek(SeekFrom::Start(0))?;
    let mut buf = [0u8; HEADER_LEN as usize];
    let n = read_up_to(file, &mut buf)?;
    if n < HEADER_LEN as usize
        || buf[0..4] != WAL_MAGIC
        || u16::from_le_bytes([buf[4], buf[5]]) != WAL_VERSION
    {
        return Err(WalError::BadHeader);
    }
    let number = u64::from_le_bytes(buf[6..14].try_into().expect("8 bytes"));
    let base_pos = u64::from_le_bytes(buf[14..22].try_into().expect("8 bytes"));
    if number != expected_number {
        return Err(WalError::BadHeader);
    }
    Ok((number, base_pos))
}

/// Replays the log's **durable prefix** — the records lying wholly below `sync_point` — and
/// discards whatever follows it. Returns the records collected and the offset replay stopped at,
/// which is the file's logical length once the tail has been truncated away.
///
/// Every failure inside the prefix is corruption of acknowledged state and returns
/// [`WalError::WalCorruption`]; see the module doc for why the answer is uniform here and uniform
/// the other way past the boundary.
fn replay(file: &mut File, sync_point: u64) -> Result<(Vec<(u64, WalRecord)>, u64)> {
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

        records.push((pos, record));
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

/// Creates member `number` of `base`, headered, sidecarred and durable — including its directory
/// entry, without which a crash immediately after creation can lose the file entirely while its
/// sidecar still claims a sync point (C3).
///
/// **A member begins with a recorded durable boundary of "header only".** Without this, a file that
/// is created, appended to, and then denied its first fsync has no sidecar at all, and the
/// missing-sidecar default (everything present is acked — see the module doc) would read back as
/// acked exactly the records that failure told the caller it did not have. That default is right
/// for a sidecar that was *lost* and must not be reached by a file that never had one.
///
/// `create_new` rather than `create`: [`SequenceBase::members`] has already established that this
/// number is free, so a file appearing under it is a same-named predecessor or a concurrent writer
/// — neither of which may be silently written over.
fn create_member(base: &SequenceBase, number: u64, base_pos: u64) -> Result<WalFile> {
    let path = base.member(number);
    let sync_path = base.sidecar(number);
    let mut file = OpenOptions::new()
        .create_new(true)
        .read(true)
        .write(true)
        .open(&path)?;
    write_header(&mut file, number, base_pos)?;
    write_sync_offset(&sync_path, HEADER_LEN)?;
    fsync_dir(&base.dir)?;
    Ok(WalFile {
        number,
        sync_path,
        file,
        base_pos,
        len: HEADER_LEN,
        durable_len: HEADER_LEN,
    })
}

impl Wal {
    /// Opens (creating if absent) the WAL sequence based at `path`, replays every surviving member
    /// under the positional CRC rule, and returns the live handle plus every record recovered.
    ///
    /// **Recovery walks every surviving file in sequence order.** It does not start *at* the
    /// newest overlay snapshot and resume: an older member can still carry `Change` records above
    /// the point that snapshot was taken at, and skipping them looks like an optimisation and is a
    /// silent un-deny.
    ///
    /// Every lifecycle §4 rule applies **per member, unchanged**: its own fsync-offset sidecar
    /// (decision 0038), the positional CRC rule, the three sidecar guards, and truncate-and-fsync
    /// before the handle is issued.
    ///
    /// Two sequence-level failures are added, and both fail closed. A **gap** in the numbering
    /// means a member was deleted out of order or lost, so records a caller was told were durable
    /// are missing with nothing to mark their absence — this is why reclamation deletes oldest
    /// first. A **broken position chain** — a member whose header base position does not continue
    /// its predecessor's durable end — means a stale or foreign file has taken a member's place,
    /// and every position derived from it afterwards would name the wrong bytes.
    pub fn open<P: AsRef<Path>>(path: P) -> Result<(Wal, Vec<WalRecord>)> {
        let base = SequenceBase::of(path.as_ref());
        let members = base.members()?;

        if members.is_empty() {
            let active = create_member(&base, 1, 0)?;
            return Ok((
                Wal {
                    base,
                    sealed: Vec::new(),
                    active,
                    state: WalState::Healthy,
                    replayed_positions: Vec::new(),
                },
                Vec::new(),
            ));
        }

        let first = members[0];
        let last = *members.last().expect("non-empty");
        if last - first + 1 != members.len() as u64 {
            return Err(WalError::WalCorruption);
        }

        let mut records = Vec::new();
        let mut sealed = Vec::new();
        let mut active = None;
        // `None` for the oldest surviving member: reclamation has removed whatever preceded it, so
        // its base position is taken as given and only its successors have anything to continue.
        let mut expected_base: Option<u64> = None;

        for (i, number) in members.iter().copied().enumerate() {
            let is_last = i + 1 == members.len();
            let path = base.member(number);
            let sync_path = base.sidecar(number);
            let mut file = OpenOptions::new().read(true).write(true).open(&path)?;

            let mut file_len = file.metadata()?.len();
            if file_len == 0 {
                // A crash between `create_new` and the header write. The member's number comes from
                // its name and its base position from the chain, so this is exactly recoverable —
                // and refusing it would turn a benign crash into a permanently unopenable node,
                // which is the outcome §7.3's oldest-first rule exists to avoid.
                let base_pos = match (expected_base, members.len()) {
                    (Some(expected), _) => expected,
                    (None, 1) => 0,
                    // An empty *oldest* member with successors: its span is unknowable, and every
                    // later member's position would be a guess.
                    (None, _) => return Err(WalError::WalCorruption),
                };
                write_header(&mut file, number, base_pos)?;
                write_sync_offset(&sync_path, HEADER_LEN)?;
                file_len = HEADER_LEN;
            }

            let (_, base_pos) = check_header(&mut file, number)?;
            if expected_base.is_some_and(|expected| expected != base_pos) {
                return Err(WalError::WalCorruption);
            }

            let sync_point = resolve_sync_point(&sync_path, file_len)?;
            let (member_records, len) = replay(&mut file, sync_point)?;
            // File offsets become sequence-global positions here, at the one place both terms are
            // in hand: a member's records are `base_pos + (offset - HEADER_LEN)` into the sequence.
            records.extend(
                member_records
                    .into_iter()
                    .map(|(offset, record)| (base_pos + (offset - HEADER_LEN), record)),
            );
            expected_base = Some(base_pos + (len - HEADER_LEN));

            if is_last {
                file.seek(SeekFrom::Start(len))?;
                active = Some(WalFile {
                    number,
                    sync_path,
                    file,
                    base_pos,
                    len,
                    // Replay ends exactly at the sync point (every record is bounded against it),
                    // and the tail past it has just been truncated away, so the file's length *is*
                    // the durable boundary at the moment a handle is issued.
                    durable_len: len,
                });
            } else {
                sealed.push(SealedSpan {
                    number,
                    end_pos: base_pos + (len - HEADER_LEN),
                });
            }
        }

        let (replayed_positions, records): (Vec<u64>, Vec<WalRecord>) = records.into_iter().unzip();

        Ok((
            Wal {
                base,
                sealed,
                active: active.expect("the last member is always the active one"),
                state: WalState::Healthy,
                replayed_positions,
            },
            records,
        ))
    }

    /// The sequence-global position of each record `open` replayed, in the same order as the
    /// records it returned.
    ///
    /// Parallel to the records rather than zipped into them because every other consumer of a
    /// replayed record — `overlay::replay`, `high_water_from` — wants the record alone, and a tuple
    /// would put a position into six signatures to serve one caller. That caller is
    /// `WritePath::reconstruct`, which stamps each buffered row with the position it arrived at so a
    /// later rotation knows what it may reclaim below.
    pub fn replayed_positions(&self) -> &[u64] {
        &self.replayed_positions
    }

    /// The sequence-global position the next record will be written at.
    ///
    /// Positions count **record bytes only**, across every member the sequence has ever held, so
    /// they are comparable after reclamation has deleted the files the earlier ones lived in. This
    /// is the quantity a flush records as its `wal_pos` — the buffer-snapshot point, below which
    /// every ingest row has been consumed into a segment.
    pub fn position(&self) -> u64 {
        self.active.base_pos + (self.active.len - HEADER_LEN)
    }

    /// The sequence-global position below which everything is durable and acknowledged.
    pub fn durable_position(&self) -> u64 {
        self.active.end_pos()
    }

    /// Every surviving member's number, ascending — the active one last. Telemetry and tests: the
    /// steady state is two, and a sequence that keeps growing is a rotation that is not reclaiming.
    pub fn members(&self) -> Vec<u64> {
        self.sealed
            .iter()
            .map(|s| s.number)
            .chain(std::iter::once(self.active.number))
            .collect()
    }

    /// Seal the active member, open the next one with `snapshot` at its head, and reclaim every
    /// member lying wholly below `reclaim_below`. Returns the numbers deleted, oldest first.
    ///
    /// ## The order, and why each step is where it is
    ///
    /// ```text
    /// fsync the active member                     ← its durable length is the next base position
    /// create member n+1, headered and fsynced
    /// append the overlay snapshot, fsynced        ← §7.2, and before any deletion
    /// delete members wholly below reclaim_below,  ← oldest first
    ///   oldest first
    /// ```
    ///
    /// **The snapshot is durable before anything is deleted**, because the overlay's only durable
    /// home is the WAL: the `Change` records inside the members about to go are the sole record
    /// that an item was suppressed, and a suppression retires only on unsuppress. Deleting first
    /// and snapshotting after would re-expose every denied item on the next restart.
    ///
    /// **Deletion is oldest-first**, because a crash midway through an unordered deletion leaves a
    /// *gap* in the sequence, and [`Wal::open`] fails closed on a gap — turning a benign crash into
    /// a permanently unopenable node. Deleting a prefix leaves a shorter sequence, which is exactly
    /// what a completed rotation leaves.
    ///
    /// **A member is reclaimable only if it lies wholly below `reclaim_below`.** Steady-state
    /// retention is therefore two members: the one holding the snapshot point is generally still
    /// live above it, so it survives its own rotation.
    ///
    /// An **empty** overlay still writes the record. The alternative is a conditional whose only
    /// benefit is a few bytes, and whose cost is that the snapshot's absence stops meaning
    /// "nothing was denied" and starts meaning either that or "no rotation happened here".
    ///
    /// A poisoned handle rotates nothing — nothing above the last durable offset may be built on,
    /// and the caller has a repair to attempt first.
    pub fn rotate(
        &mut self,
        snapshot: &[OverlaySnapshotEntry],
        reclaim_below: u64,
    ) -> Result<Vec<u64>> {
        if self.state != WalState::Healthy {
            return Err(WalError::Poisoned);
        }
        if self.active.len != self.active.durable_len {
            self.sync_and_publish()?;
        }

        let sealed_end = self.active.end_pos();
        let next = create_member(&self.base, self.active.number + 1, sealed_end)?;
        let previous = std::mem::replace(&mut self.active, next);
        self.sealed.push(SealedSpan {
            number: previous.number,
            end_pos: sealed_end,
        });
        drop(previous);

        self.append(&WalRecord::OverlaySnapshot {
            entries: snapshot.to_vec(),
        })?;
        self.fsync()?;

        let mut deleted = Vec::new();
        while let Some(span) = self.sealed.first().copied() {
            if span.end_pos > reclaim_below {
                break;
            }
            // The file before its sidecar, and the directory fsynced after each: a crash between
            // the two leaves a sidecar naming a member that is gone, which `members()` does not
            // list and nothing reads. The reverse leaves a member with no sidecar, which the
            // missing-sidecar default would then read as "everything present is acked" — sound, but
            // it is state this rotation has already decided is redundant.
            std::fs::remove_file(self.base.member(span.number))?;
            match std::fs::remove_file(self.base.sidecar(span.number)) {
                Ok(()) => {}
                Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
                Err(e) => return Err(e.into()),
            }
            fsync_dir(&self.base.dir)?;
            self.sealed.remove(0);
            deleted.push(span.number);
        }
        Ok(deleted)
    }

    /// Buffers `rec` for append. Not durable until [`Wal::fsync`] returns.
    pub fn append(&mut self, rec: &WalRecord) -> Result<()> {
        if self.state != WalState::Healthy {
            return Err(WalError::Poisoned);
        }
        let body = postcard::to_allocvec(rec)?;
        match self.write_framed(&body) {
            Ok(()) => {
                self.active.len += framed_len(body.len());
                Ok(())
            }
            Err(e) => {
                // A partially-completed write_all may have left the file at an offset between
                // the old and new `self.active.len` — we cannot know how many bytes actually landed, so
                // `self.active.len` can no longer be trusted to name a record boundary. Poison rather
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
        self.active
            .file
            .write_all(&(body.len() as u32).to_le_bytes())?;
        self.active.file.write_all(body)?;
        self.active.file.write_all(&crc.to_le_bytes())?;
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

    /// Discard everything above the last durable offset and return the handle to service.
    ///
    /// ## What this is, and why it is a *discard* rather than a second repair
    ///
    /// [`Wal::retry_durability`] rescues a window whose caller has not yet been answered. Once that
    /// window has been answered — with an error saying its write is not durable — the region above
    /// `durable_len` becomes something else entirely: **bytes every caller has been told do not
    /// count.** Making them durable afterwards is the fail-open the module doc argues against at
    /// the top, reached from the other end. A refused ingest would reappear, and a deny window's
    /// `unsuppress` — appended like every other entry, but deliberately *not* applied in memory
    /// (lifecycle §4 scopes the apply-anyway rule to deletion and suppression) — would take effect
    /// at the next replay, un-hiding an item whose operator was told it was still suppressed.
    ///
    /// So the recovery is the other direction: truncate to `durable_len`, which is **exactly what a
    /// restart does with the same file**. `Wal::open` resolves the sidecar, replays the prefix and
    /// truncates the tail; this performs that same truncation in place, without the restart. A
    /// handle recovered this way is therefore in a state some restart could have produced, which is
    /// the strongest safety argument available for any in-process recovery.
    ///
    /// It needs no records, so nothing has to be retained to make it possible, and it treats
    /// [`WalState::Unsynced`] and [`WalState::Unpublished`] identically — the bytes are discarded
    /// whether or not they reached the device.
    ///
    /// ## What it deliberately does not do
    ///
    /// It does not recover [`WalState::Torn`]. Truncating to `durable_len` would in fact restore a
    /// torn handle too — `durable_len` is a record boundary, and a partial `write_all` can only
    /// have landed above it — but a torn handle is the one state in which what the file contains and
    /// where the descriptor sits are both unknown, and it is the state this module has always
    /// treated as terminal. Widening it is a decision with an owner, not a consequence of this one.
    ///
    /// It does not un-apply anything. Deletions and suppressions applied under the apply-anyway rule
    /// stay applied in memory for as long as the process lives; this only makes the log agree with
    /// what those callers were told, which is that a restart will not carry them.
    pub fn discard_undurable(&mut self) -> Result<u64> {
        if self.state == WalState::Torn {
            return Err(WalError::Poisoned);
        }
        if self.state == WalState::Healthy && self.active.len == self.active.durable_len {
            return Ok(self.active.durable_len);
        }
        // Every other case does the same work, including a *healthy* handle carrying appends that
        // have not been synced. Keying the discard on the region rather than on the state is what
        // keeps this honest under fault injection, where the poison is held by the wrapper and the
        // underlying handle is healthy with exactly such a region: a version that returned early on
        // `Healthy` would leave those bytes in place, and the next window's fsync would sweep the
        // refused records into the durable prefix — the fail-open this whole function exists to
        // avoid, reintroduced through the test harness.

        // **The boundary comes down before the bytes do, and the order is load-bearing.** In
        // `Unpublished` the sidecar's rename may already have completed — only its directory fsync
        // failed — so the sidecar can name the *higher* offset. Truncating first and crashing before
        // republishing would leave a log shorter than its own sync point, which `open` refuses
        // outright (C1): a recovery that can produce an unopenable node is not a recovery.
        //
        // Republishing first is safe in every state, including the one where it changes nothing:
        // lowering the recorded boundary to a value that was already durable only ever discards
        // more, and discarding is what this call is for. A crash between the two steps leaves the
        // sidecar naming `durable_len` and the file longer, which is the ordinary tail `open`
        // truncates away.
        write_sync_offset(&self.active.sync_path, self.active.durable_len).map_err(WalError::Io)?;
        self.active
            .file
            .set_len(self.active.durable_len)
            .map_err(WalError::Io)?;
        self.active.file.sync_data().map_err(WalError::Io)?;
        self.active
            .file
            .seek(SeekFrom::Start(self.active.durable_len))
            .map_err(WalError::Io)?;

        self.active.len = self.active.durable_len;
        self.state = WalState::Healthy;
        Ok(self.active.durable_len)
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
        if self.active.durable_len + total != self.active.len {
            // Refuse rather than write a region whose extent we cannot predict — and leave the
            // state alone, so the handle goes on refusing everything.
            return Err(WalError::Poisoned);
        }

        if self.state == WalState::Unsynced {
            let rewrite: std::io::Result<()> = (|| {
                self.active
                    .file
                    .seek(SeekFrom::Start(self.active.durable_len))?;
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
        if let Err(e) = self.active.file.sync_data() {
            self.state = WalState::Unsynced;
            return Err(WalError::Io(e));
        }

        match write_sync_offset(&self.active.sync_path, self.active.len) {
            Ok(()) => {
                self.active.durable_len = self.active.len;
                self.state = WalState::Healthy;
                Ok(self.active.len)
            }
            Err(e) => {
                self.state = WalState::Unpublished;
                Err(WalError::Io(e))
            }
        }
    }
}

/// The write executor's **sole** WAL handle: a [`Wal`] owned by value, plus the counters and — in
/// test builds — the fault switches that make the ack contract observable.
///
/// ## Why the executor holds this rather than a bare `Wal`
///
/// Two things need to hang off every append and fsync, and neither belongs inside [`Wal`]:
///
/// - **The counters** ([`WalMeter`]), which are production telemetry. Group commit is
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
/// from the WAL cannot. This is the value the executor's not-ready posture is built on
/// (lifecycle §4).
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
    /// executor thread — and that single ownership *is* the ordering guarantee. The alternative is
    /// a `Mutex<Wal>` held across append→fsync→apply→swap, where the ordering is a discipline a
    /// reader has to be told about rather than one the type enforces.
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

    /// The sequence-global position the next record will be written at — see [`Wal::position`].
    /// Read *before* an append to learn where that record will land.
    pub fn position(&self) -> u64 {
        self.wal.position()
    }

    /// Seal the active member, carry `snapshot` forward and reclaim below `reclaim_below` — see
    /// [`Wal::rotate`]. Not metered: a rotation makes nothing newly durable that an `fsync` did not
    /// already count.
    pub fn rotate(
        &mut self,
        snapshot: &[OverlaySnapshotEntry],
        reclaim_below: u64,
    ) -> Result<Vec<u64>> {
        #[cfg(feature = "fault-injection")]
        if self.injected.is_some() {
            return Err(WalError::Poisoned);
        }
        self.wal.rotate(snapshot, reclaim_below)
    }

    /// Every surviving member's number — see [`Wal::members`].
    pub fn members(&self) -> Vec<u64> {
        self.wal.members()
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

    /// Discard the undurable region and return to service — see [`Wal::discard_undurable`], which
    /// this is the fault-injectable face of.
    ///
    /// Not counted by the meter: nothing was made durable, which is the whole point of it.
    pub fn discard_undurable(&mut self) -> Result<u64> {
        #[cfg(feature = "fault-injection")]
        {
            match self.injected {
                // A torn handle does not recover, injected or real.
                Some(InjectedPoison::Torn) => return Err(WalError::Poisoned),
                // An injected sync failure left the real `Wal` untouched, so there is no undurable
                // region on disk for it to discard — but the *handle* must come back, or an injected
                // failure would be terminal where a real one is not.
                Some(InjectedPoison::Recoverable) => self.injected = None,
                None => {}
            }
        }
        self.wal.discard_undurable()
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
        WalRecord::ChangeByEntity {
            entity_id: EntityId::new(tag as u64),
            op: ChangeOp::Delete,
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
            let durable = wal.active.durable_len;

            wal.append(&record(1)).unwrap();
            wal.append(&record(2)).unwrap();
            let end = wal.active.len;

            // The sync failed, and the pages it was meant to write are gone.
            wal.state = WalState::Unsynced;
            blank_region(&wal.base.member(wal.active.number), durable, end);

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

    /// **A torn handle recovers by neither route.** Pinned here, against a real [`WalState::Torn`],
    /// because the wrapper's injected-fault arm refuses first and so hides whatever this function
    /// does — `tests/wal.rs`'s injected case asserts the wrapper, and asserting the same property
    /// twice at the same layer is not defence in depth, it is one test written twice.
    ///
    /// Truncating to `durable_len` would in fact restore this handle: that offset is a record
    /// boundary and a partial `write_all` can only have landed above it. The refusal is a chosen
    /// conservatism about the one state in which neither the file's contents nor the descriptor's
    /// position is known — not an impossibility, and widening it is an owner's decision.
    #[test]
    fn a_torn_handle_recovers_by_neither_route() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("wal.log");
        let (mut wal, _) = Wal::open(&path).unwrap();
        wal.append(&record(0)).unwrap();
        wal.fsync().unwrap();
        wal.append(&record(1)).unwrap();

        wal.state = WalState::Torn;

        assert!(matches!(wal.discard_undurable(), Err(WalError::Poisoned)));
        assert!(matches!(
            wal.retry_durability(&[record(1)]),
            Err(WalError::Poisoned)
        ));
        assert!(wal.is_poisoned(), "and it stays poisoned");
        assert!(!wal.is_recoverable(), "and it never claims otherwise");
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
            let durable = wal.active.durable_len;

            wal.append(&record(1)).unwrap();
            let end = wal.active.len;

            // `Unpublished` is the state that skips the rewrite, so this is the bare re-sync.
            wal.state = WalState::Unpublished;
            blank_region(&wal.base.member(wal.active.number), durable, end);

            wal.retry_durability(&[record(1)])
                .expect("a bare re-sync reports success — that is the whole trap");
        }

        assert!(
            matches!(Wal::open(&path), Err(WalError::WalCorruption)),
            "a hole published inside the durable prefix must refuse to open"
        );
    }
}
