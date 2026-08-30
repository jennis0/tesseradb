//! The packed membership extent: many artifact memberships behind **one** manifest entry.
//!
//! # Why this file exists at all
//!
//! `annotation-representation.md` §2.4 sketches one membership file per artifact and then marks the
//! line *"a shape, not a layout"*: every bundle file is a manifest entry, digested at write and
//! parsed at open, so 10⁷ artifacts would be 10⁷ entries. The bytes are affordable and the
//! *packaging* is not. This is the packaging — one file per publication per level, addressed by a
//! dense ordinal, exactly as the record blob's directory is the precedent for many small objects
//! behind one entry.
//!
//! Until it existed, a membership's only durable home was the WAL, and rotation was pinned from the
//! first publication onwards so the log could never be reclaimed.
//!
//! # The extent format knows nothing about bitmaps
//!
//! A membership is an opaque byte blob in the extent format, and deliberately: `tessera-lifecycle`
//! owns the Roaring form and does not depend on this crate, so a layout that understood the payload
//! would put the bitmap library on both sides of a boundary that currently has it on one. What the
//! format owns is **addressing** — which bytes belong to which ordinal — and nothing else.
//!
//! That is true of this file without qualification. The one part of the prefix's artifact
//! machinery that *does* read bitmaps — the pass producing the inputs to the formats below — is
//! [`crate::derived`], a file of its own for exactly this reason: a module whose doc claims it
//! knows nothing about the payload should not hold nine hundred lines that do. The two stay in one
//! crate on `tessera-build`'s own rule for the filter artefact — the format's owner owns both
//! halves, so the writer and the reader cannot drift — and the boundary [`crate::derived`] keeps
//! is the other one: it never sees an `ArtifactRecord`.
//!
//! # The format
//!
//! ```text
//! header  := magic "TSMB" | u16 version | u16 reserved | u32 count | u32 ordinal_lo
//! offsets := u64 LE × (count + 1)   -- into the payload region; offsets[0] == 0, ascending
//! payload := count blobs, concatenated in ordinal order
//! ```
//!
//! **Offsets rather than lengths, and one more of them than there are blobs.** A length array needs
//! a running sum to address the *n*th blob; an offset array addresses it in two reads, which is what
//! makes a future mapped reader a change of how the file is read rather than of what is written. The
//! extra trailing offset is the payload's end, so the last blob needs no special case — the case a
//! reader gets wrong.
//!
//! **`ordinal_lo` is in the file and not only in the manifest.** A publication appends the ordinals
//! that arrived since the last one, so an extent covers `[ordinal_lo, ordinal_lo + count)`. Carrying
//! it here means a file and the manifest entry naming it can be checked against each other, and a
//! mismatch refuses — where a file that only knew its own length would silently answer for whichever
//! range it was asked about.
//!
//! # Fail-closed
//!
//! Bad magic, an unknown version, a short file, non-ascending offsets, or a final offset that
//! disagrees with the payload length all **refuse**. None of them decode to a plausible shorter
//! extent, because a membership that came back short is an artifact whose masked count is low for
//! every viewer — which the existence criterion then renders as *absent*, indistinguishable from a
//! cluster that legitimately failed its bar. There is no error to notice downstream, so it has to be
//! caught here.
//!
//! Corruption *inside* a blob is caught by the caller's own decoder rather than here: the shipped
//! reader hands each blob to Roaring's checked deserialiser, which refuses bytes that are not a
//! bitmap. That is why this format carries no digest of its own — the payload validates itself, and
//! the framing above is what stops a truncation being read as a smaller extent.

use std::fs::File;
use std::io::{BufWriter, Seek, SeekFrom, Write};
use std::path::{Path, PathBuf};

use memmap2::Mmap;

use crate::error::{Result, StoreError};

const MAGIC: &[u8; 4] = b"TSMB";
/// Bumped whenever a blob's *content* changes shape, even though this module holds a blob opaquely:
/// the refusal has to happen at the file, because the decoder on the other side of the boundary
/// sees only bytes. Version 2 is the artifact record carrying its attachment — an extent written
/// under version 1 restores every label as unattached, which serves the labels of suppressed
/// clusters.
const VERSION: u16 = 2;
const HEADER_LEN: usize = 4 + 2 + 2 + 4 + 4;

fn malformed(path: &Path, detail: impl std::fmt::Display) -> StoreError {
    StoreError::MalformedBundle {
        detail: format!("membership extent {}: {detail}", path.display()),
    }
}

/// Serialise one extent: `blobs` in ordinal order, covering `[ordinal_lo, ordinal_lo + blobs.len())`.
///
/// Returns the bytes rather than writing them, so the caller owns the durability sequence — the file
/// must be fsynced **before** the manifest that names it, and only the caller knows where that
/// manifest write sits.
pub fn pack(ordinal_lo: u32, blobs: &[Vec<u8>]) -> Vec<u8> {
    let count = blobs.len();
    let payload_len: usize = blobs.iter().map(Vec::len).sum();
    let mut out = Vec::with_capacity(HEADER_LEN + (count + 1) * 8 + payload_len);

    out.extend_from_slice(MAGIC);
    out.extend_from_slice(&VERSION.to_le_bytes());
    out.extend_from_slice(&0u16.to_le_bytes());
    out.extend_from_slice(&(count as u32).to_le_bytes());
    out.extend_from_slice(&ordinal_lo.to_le_bytes());

    let mut at = 0u64;
    out.extend_from_slice(&at.to_le_bytes());
    for blob in blobs {
        at += blob.len() as u64;
        out.extend_from_slice(&at.to_le_bytes());
    }
    for blob in blobs {
        out.extend_from_slice(blob);
    }
    out
}

/// Serialise one extent **a blob at a time**, straight to the file — [`pack`]'s bytes, without
/// ever holding them.
///
/// # Why a second writer
///
/// [`pack`] takes every blob of a level at once and returns their concatenation, so at the moment
/// it returns, a level's memberships stand in memory twice over. That is affordable for a
/// publication that appends the tail one commit produced, and it is not affordable for a **build**,
/// which publishes a whole corpus's levels in one pass and holds the encoded blobs of all of them
/// while it does. The two callers are also not free to share one route: the online publication
/// reads the artifact store under a lock it must not hold across an fsync, so it materialises and
/// releases; a build owns its store outright and can encode as it writes.
///
/// **The count is a contract, not a hint.** It is the level's ordinal range, known before the first
/// blob, which is what lets the offset table be reserved and filled in afterwards. [`Self::push`]
/// refuses the blob past the end and [`Self::finish`] refuses a writer that was pushed fewer than
/// it reserved: an extent addresses `[ordinal_lo, ordinal_lo + count)` densely, so a short one is a
/// membership that decodes as absent for every viewer — which the existence criterion then renders
/// as a cluster that legitimately failed its bar.
pub struct PackWriter {
    path: PathBuf,
    file: BufWriter<File>,
    ordinal_lo: u32,
    count: u32,
    /// Where each blob starts within the payload, and one more for the payload's end — the file's
    /// own table, held here at 8 bytes an artifact until the payload has gone past and it can be
    /// written back over the space reserved for it.
    offsets: Vec<u64>,
}

impl PackWriter {
    /// Reserve the header and the offset table for `count` blobs, and position at the payload.
    pub fn create(path: &Path, ordinal_lo: u32, count: u32) -> Result<PackWriter> {
        let file = File::create(path).map_err(|source| StoreError::Io {
            path: path.to_path_buf(),
            source,
        })?;
        let mut file = BufWriter::new(file);
        let prologue = HEADER_LEN + (count as usize + 1) * 8;
        file.write_all(&vec![0u8; prologue])
            .map_err(|source| StoreError::Io {
                path: path.to_path_buf(),
                source,
            })?;
        let mut offsets = Vec::with_capacity(count as usize + 1);
        offsets.push(0);
        Ok(PackWriter {
            path: path.to_path_buf(),
            file,
            ordinal_lo,
            count,
            offsets,
        })
    }

    /// Append one blob, in ordinal order.
    pub fn push(&mut self, blob: &[u8]) -> Result<()> {
        if self.offsets.len() > self.count as usize {
            return Err(malformed(
                &self.path,
                format!(
                    "a {}-blob extent was pushed a {}th blob",
                    self.count,
                    self.offsets.len()
                ),
            ));
        }
        self.file.write_all(blob).map_err(|source| StoreError::Io {
            path: self.path.clone(),
            source,
        })?;
        let at = self.offsets.last().copied().unwrap_or_default() + blob.len() as u64;
        self.offsets.push(at);
        Ok(())
    }

    /// Write the header and the offset table over the space reserved for them, and fsync.
    ///
    /// The file is durable when this returns — the caller still owns the *directory* sync and the
    /// ordering against the manifest that names it, exactly as with [`crate::write_and_fsync`].
    pub fn finish(self) -> Result<()> {
        let PackWriter {
            path,
            file,
            ordinal_lo,
            count,
            offsets,
        } = self;
        if offsets.len() != count as usize + 1 {
            return Err(malformed(
                &path,
                format!(
                    "{} blob(s) were written into an extent reserved for {count} — the ordinal \
                     range it addresses would decode as absent above the last one",
                    offsets.len() - 1,
                ),
            ));
        }
        let mut head = Vec::with_capacity(HEADER_LEN + offsets.len() * 8);
        head.extend_from_slice(MAGIC);
        head.extend_from_slice(&VERSION.to_le_bytes());
        head.extend_from_slice(&0u16.to_le_bytes());
        head.extend_from_slice(&count.to_le_bytes());
        head.extend_from_slice(&ordinal_lo.to_le_bytes());
        for at in &offsets {
            head.extend_from_slice(&at.to_le_bytes());
        }
        let mut file = file.into_inner().map_err(|e| StoreError::Io {
            path: path.clone(),
            source: e.into_error(),
        })?;
        let io = |source| StoreError::Io {
            path: path.clone(),
            source,
        };
        file.seek(SeekFrom::Start(0)).map_err(io)?;
        file.write_all(&head).map_err(io)?;
        file.sync_all().map_err(io)
    }
}

/// One extent, opened and validated. The mapping is held for the reader's life; blobs are slices
/// into it, so nothing is copied until a caller decodes one.
pub struct MembershipPack {
    map: Mmap,
    count: u32,
    ordinal_lo: u32,
    /// Byte offset of `offsets[0]` within the mapping.
    offsets_at: usize,
    /// Byte offset of the payload region within the mapping.
    payload_at: usize,
}

impl MembershipPack {
    /// Open and validate. Every check here refuses rather than truncating — see the module doc.
    pub fn open(path: &Path) -> Result<Self> {
        let file = File::open(path).map_err(|source| StoreError::Io {
            path: path.to_path_buf(),
            source,
        })?;
        // SAFETY: the file is opened read-only and the mapping is never written through. A
        // concurrent truncation would be undefined, and nothing truncates a published extent: a
        // prefix's files are written once and removed only by reclamation, which runs when no
        // generation names them.
        let map = unsafe { Mmap::map(&file) }.map_err(|source| StoreError::Io {
            path: path.to_path_buf(),
            source,
        })?;

        if map.len() < HEADER_LEN {
            return Err(malformed(
                path,
                format!(
                    "{} bytes is shorter than the {HEADER_LEN}-byte header",
                    map.len()
                ),
            ));
        }
        if &map[0..4] != MAGIC {
            return Err(malformed(path, "magic is not TSMB"));
        }
        let version = u16::from_le_bytes([map[4], map[5]]);
        if version != VERSION {
            return Err(malformed(
                path,
                format!("version {version}, expected {VERSION}"),
            ));
        }
        let count = u32::from_le_bytes([map[8], map[9], map[10], map[11]]);
        let ordinal_lo = u32::from_le_bytes([map[12], map[13], map[14], map[15]]);

        let offsets_at = HEADER_LEN;
        let offsets_len = (count as usize + 1)
            .checked_mul(8)
            .ok_or_else(|| malformed(path, format!("count {count} overflows the offset table")))?;
        let payload_at = offsets_at
            .checked_add(offsets_len)
            .ok_or_else(|| malformed(path, "offset table overflows the file"))?;
        if map.len() < payload_at {
            return Err(malformed(
                path,
                format!(
                    "{} bytes cannot hold {count} offsets: the file is truncated",
                    map.len()
                ),
            ));
        }

        let pack = MembershipPack {
            map,
            count,
            ordinal_lo,
            offsets_at,
            payload_at,
        };

        // Ascending offsets, and a final one that is exactly the payload length. Checked once, at
        // open, so every `blob` below is a slice of a range already proven inside the mapping —
        // which is what lets that function be infallible.
        let mut previous = 0u64;
        for i in 0..=count as usize {
            let offset = pack.offset(i);
            if offset < previous {
                return Err(malformed(
                    path,
                    format!("offset {i} goes backwards ({offset} after {previous})"),
                ));
            }
            previous = offset;
        }
        let payload_len = pack.map.len() - payload_at;
        if previous != payload_len as u64 {
            return Err(malformed(
                path,
                format!(
                    "the last offset is {previous} but the payload is {payload_len} bytes — the \
                     file is truncated or was written by a different packer"
                ),
            ));
        }

        Ok(pack)
    }

    fn offset(&self, i: usize) -> u64 {
        let at = self.offsets_at + i * 8;
        u64::from_le_bytes(self.map[at..at + 8].try_into().expect("8 bytes"))
    }

    /// The first ordinal this extent covers.
    pub fn ordinal_lo(&self) -> u32 {
        self.ordinal_lo
    }

    /// How many ordinals this extent covers.
    pub fn count(&self) -> u32 {
        self.count
    }

    /// One membership's bytes, by **absolute** ordinal — not by index within the extent.
    ///
    /// Taking the absolute ordinal is what stops a caller iterating several extents and addressing
    /// the second one from zero, which would serve one artifact's members under another's identity.
    /// `None` for an ordinal this extent does not cover.
    pub fn blob(&self, ordinal: u32) -> Option<&[u8]> {
        let i = ordinal.checked_sub(self.ordinal_lo)? as usize;
        if i >= self.count as usize {
            return None;
        }
        let from = self.payload_at + self.offset(i) as usize;
        let to = self.payload_at + self.offset(i + 1) as usize;
        Some(&self.map[from..to])
    }

    /// Every membership in this extent, as `(ordinal, bytes)` in ordinal order.
    pub fn iter(&self) -> impl Iterator<Item = (u32, &[u8])> {
        (0..self.count).map(move |i| {
            let ordinal = self.ordinal_lo + i;
            (ordinal, self.blob(ordinal).expect("within this extent"))
        })
    }
}

// ---- the containment partition -------------------------------------------------------------
//
// A second format, in this module because it is the same family: a per-generation artifact
// structure written into the prefix at the fold, addressed by dense ordinal, and served **mapped**
// rather than deserialised into anonymous memory (the layout memo's constraint 13). It gets its
// own magic and its own version, so a reader handed one where the other belongs refuses instead of
// finding a plausible header.
//
// Unlike the membership extent above, this module *does* understand the payload — there is no
// bitmap in it, only four integer arrays, and the framing checks below are exactly what make the
// accessors infallible.
//
// ```text
// header  := magic "TSCP" | u16 version | u16 id_width | u32 ordinals | u32 pairs
//                         | u32 expressions | u32 words
// at      := u32 LE x (ordinals + 1)   -- into `ids`; at[0] == 0, ascending, at[ordinals] == pairs
// ids     := id_width bytes x pairs, padded to a 4-byte boundary
// expr_at := u32 LE x (expressions + 1) -- into `words`; expr_at[0] == 0, ascending
// words   := u32 LE x words            -- per expression: nclauses, (len, terms...)*
// ```
//
// **Every array is 4-byte framed and read one integer at a time** rather than transmuted from the
// mapping. A mapped `&[u32]` would need an alignment guarantee the format cannot make once the
// narrow id column has an odd length, and the alternative — padding the reader into an assumption
// — is how a file written on one machine comes to be read wrongly on another.

const CONTAINMENT_MAGIC: &[u8; 4] = b"TSCP";
/// Bumped whenever the array layout or the word encoding changes. There is no compatibility to
/// keep (decision 0048); the number exists so a stale local file is a loud refusal rather than a
/// silent misread of a containment answer.
const CONTAINMENT_VERSION: u16 = 1;
const CONTAINMENT_HEADER_LEN: usize = 4 + 2 + 2 + 4 + 4 + 4 + 4;

fn containment_malformed(what: &str, detail: impl std::fmt::Display) -> StoreError {
    StoreError::MalformedBundle {
        detail: format!("containment partition {what}: {detail}"),
    }
}

/// Serialise one level's partition. `ids` are expression identifiers, `id_width` is 2 or 4 bytes.
///
/// Returns the bytes rather than writing them, for [`pack`]'s reason: the caller owns the
/// durability sequence, and only the caller knows where the manifest write naming this file sits.
pub fn pack_containment(
    id_width: u8,
    at: &[u32],
    ids: &[u32],
    expr_at: &[u32],
    words: &[u32],
) -> Vec<u8> {
    let ordinals = at.len().saturating_sub(1) as u32;
    let expressions = expr_at.len().saturating_sub(1) as u32;
    let mut out = Vec::with_capacity(CONTAINMENT_HEADER_LEN + (at.len() + words.len()) * 4);
    out.extend_from_slice(CONTAINMENT_MAGIC);
    out.extend_from_slice(&CONTAINMENT_VERSION.to_le_bytes());
    out.extend_from_slice(&u16::from(id_width).to_le_bytes());
    out.extend_from_slice(&ordinals.to_le_bytes());
    out.extend_from_slice(&(ids.len() as u32).to_le_bytes());
    out.extend_from_slice(&expressions.to_le_bytes());
    out.extend_from_slice(&(words.len() as u32).to_le_bytes());
    for value in at {
        out.extend_from_slice(&value.to_le_bytes());
    }
    for value in ids {
        match id_width {
            2 => out.extend_from_slice(&(*value as u16).to_le_bytes()),
            _ => out.extend_from_slice(&value.to_le_bytes()),
        }
    }
    while out.len() % 4 != 0 {
        out.push(0);
    }
    for value in expr_at {
        out.extend_from_slice(&value.to_le_bytes());
    }
    for value in words {
        out.extend_from_slice(&value.to_le_bytes());
    }
    out
}

/// One level's containment partition, framed and checked once at open.
///
/// **Mapped, not decoded.** At ten million artifacts the expression table and the identifier column
/// are tens of megabytes per level, and the residency campaign's direction is the whole reason:
/// page cache is reclaimable where an anonymous allocation is an OOM. [`Self::from_bytes`] exists
/// for the same structure held in memory — the form a publication builds before any fold has
/// consolidated it — so that one reader serves both and a mapped file cannot be read by rules the
/// in-memory form was never checked against.
pub struct ContainmentPack {
    bytes: ContainmentBytes,
    id_width: u8,
    ordinals: u32,
    pairs: u32,
    expressions: u32,
    words: u32,
    ids_at: usize,
    expr_at: usize,
    words_at: usize,
}

enum ContainmentBytes {
    Mapped(Mmap),
    Owned(Vec<u8>),
}

impl ContainmentBytes {
    fn as_slice(&self) -> &[u8] {
        match self {
            ContainmentBytes::Mapped(map) => map,
            ContainmentBytes::Owned(bytes) => bytes,
        }
    }
}

impl std::fmt::Debug for ContainmentPack {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ContainmentPack")
            .field("ordinals", &self.ordinals)
            .field("pairs", &self.pairs)
            .field("expressions", &self.expressions)
            .field("id_width", &self.id_width)
            .finish()
    }
}

impl ContainmentPack {
    /// Open a fold-written partition, mapped in place.
    pub fn open(path: &Path) -> Result<Self> {
        let file = File::open(path).map_err(|source| StoreError::Io {
            path: path.to_path_buf(),
            source,
        })?;
        // SAFETY: as [`MembershipPack::open`] — read-only, and nothing truncates a published
        // prefix's files while a generation names them.
        let map = unsafe { Mmap::map(&file) }.map_err(|source| StoreError::Io {
            path: path.to_path_buf(),
            source,
        })?;
        Self::frame(ContainmentBytes::Mapped(map), &path.display().to_string())
    }

    /// The same structure held in memory, checked by the same rules.
    pub fn from_bytes(bytes: Vec<u8>) -> Result<Self> {
        Self::frame(ContainmentBytes::Owned(bytes), "(in memory)")
    }

    /// Every check refuses rather than truncating, and the reason is sharper here than for a
    /// membership: a partition read short answers *contained* for a generating set whose remaining
    /// clauses were never tested, which is containment in the **permissive** direction, on the one
    /// test **I3** exists to make conservative. There is nothing downstream to notice it.
    fn frame(bytes: ContainmentBytes, what: &str) -> Result<Self> {
        let raw = bytes.as_slice();
        if raw.len() < CONTAINMENT_HEADER_LEN {
            return Err(containment_malformed(
                what,
                format!(
                    "{} bytes is shorter than the {CONTAINMENT_HEADER_LEN}-byte header",
                    raw.len()
                ),
            ));
        }
        if &raw[0..4] != CONTAINMENT_MAGIC {
            return Err(containment_malformed(what, "magic is not TSCP"));
        }
        let version = u16::from_le_bytes([raw[4], raw[5]]);
        if version != CONTAINMENT_VERSION {
            return Err(containment_malformed(
                what,
                format!("version {version}, expected {CONTAINMENT_VERSION}"),
            ));
        }
        let id_width = u16::from_le_bytes([raw[6], raw[7]]);
        if id_width != 2 && id_width != 4 {
            return Err(containment_malformed(
                what,
                format!("identifier width {id_width}, expected 2 or 4"),
            ));
        }
        let id_width = id_width as u8;
        let read = |at: usize| u32::from_le_bytes([raw[at], raw[at + 1], raw[at + 2], raw[at + 3]]);
        let ordinals = read(8);
        let pairs = read(12);
        let expressions = read(16);
        let words = read(20);

        // **A narrow column cannot address a wide table.** The promotion rule that produced the
        // file is what stops an identifier being truncated into another expression's; a file
        // claiming otherwise was not written by it.
        if id_width == 2 && expressions as usize > usize::from(u16::MAX) + 1 {
            return Err(containment_malformed(
                what,
                format!("{expressions} expressions cannot be addressed by a 2-byte identifier"),
            ));
        }

        let at_at = CONTAINMENT_HEADER_LEN;
        let ids_at = at_at + (ordinals as usize + 1) * 4;
        let expr_at = {
            let unpadded = ids_at + pairs as usize * usize::from(id_width);
            unpadded + (4 - unpadded % 4) % 4
        };
        let words_at = expr_at + (expressions as usize + 1) * 4;
        let total = words_at + words as usize * 4;
        if raw.len() != total {
            return Err(containment_malformed(
                what,
                format!(
                    "{} bytes, but the header describes {total} — the file is truncated or was \
                     written by a different packer",
                    raw.len()
                ),
            ));
        }

        let pack = ContainmentPack {
            bytes,
            id_width,
            ordinals,
            pairs,
            expressions,
            words,
            ids_at,
            expr_at,
            words_at,
        };

        if pack.at(0) != 0 || pack.at(ordinals as usize) != pairs {
            return Err(containment_malformed(
                what,
                "the ordinal offsets do not span exactly the identifier column",
            ));
        }
        for i in 0..ordinals as usize {
            if pack.at(i) > pack.at(i + 1) {
                return Err(containment_malformed(
                    what,
                    format!("ordinal offset {i} goes backwards"),
                ));
            }
        }
        if pack.expression_at(0) != 0 || pack.expression_at(expressions as usize) != words {
            return Err(containment_malformed(
                what,
                "the expression offsets do not span exactly the word array",
            ));
        }
        for i in 0..expressions as usize {
            if pack.expression_at(i) > pack.expression_at(i + 1) {
                return Err(containment_malformed(
                    what,
                    format!("expression offset {i} goes backwards"),
                ));
            }
        }
        for i in 0..pairs as usize {
            if pack.id(i) >= expressions {
                return Err(containment_malformed(
                    what,
                    format!(
                        "identifier {i} names expression {} of {expressions}",
                        pack.id(i)
                    ),
                ));
            }
        }
        // Each expression's own framing: `nclauses` followed by exactly that many `(len, terms…)`
        // runs, ending exactly where the next expression begins. An expression that framed short
        // would drop clauses, and a dropped clause is a member nobody has to be able to see.
        for id in 0..expressions as usize {
            let lo = pack.expression_at(id) as usize;
            let hi = pack.expression_at(id + 1) as usize;
            if lo >= hi {
                return Err(containment_malformed(
                    what,
                    format!("expression {id} has no clause count"),
                ));
            }
            let mut at = lo + 1;
            for _ in 0..pack.word(lo) {
                if at >= hi {
                    return Err(containment_malformed(
                        what,
                        format!("expression {id} claims more clauses than it carries"),
                    ));
                }
                at += 1 + pack.word(at) as usize;
                if at > hi {
                    return Err(containment_malformed(
                        what,
                        format!("a clause of expression {id} reaches past its own words"),
                    ));
                }
            }
            if at != hi {
                return Err(containment_malformed(
                    what,
                    format!("expression {id} carries words no clause accounts for"),
                ));
            }
        }
        Ok(pack)
    }

    /// How many ordinals this partition covers, holes included.
    pub fn ordinals(&self) -> u32 {
        self.ordinals
    }

    /// How many `(artifact, rank)` pairs the identifier column holds.
    pub fn pairs(&self) -> u32 {
        self.pairs
    }

    /// How many distinct expressions the level composed to.
    pub fn expressions(&self) -> u32 {
        self.expressions
    }

    /// Bytes per identifier — 2 or 4.
    pub fn id_width(&self) -> u8 {
        self.id_width
    }

    /// The identifier column's extent for `ordinal`: `at(o)..at(o + 1)`. Infallible for
    /// `o <= ordinals()`, which [`Self::frame`] proved.
    pub fn at(&self, ordinal: usize) -> u32 {
        let base = CONTAINMENT_HEADER_LEN + ordinal * 4;
        self.u32_at(base)
    }

    /// The expression identifier at `index` in the column.
    pub fn id(&self, index: usize) -> u32 {
        match self.id_width {
            2 => {
                let at = self.ids_at + index * 2;
                let raw = self.bytes.as_slice();
                u32::from(u16::from_le_bytes([raw[at], raw[at + 1]]))
            }
            _ => self.u32_at(self.ids_at + index * 4),
        }
    }

    /// Where expression `id`'s words begin.
    pub fn expression_at(&self, id: usize) -> u32 {
        self.u32_at(self.expr_at + id * 4)
    }

    /// One word of the expression array.
    pub fn word(&self, index: usize) -> u32 {
        self.u32_at(self.words_at + index * 4)
    }

    /// How many words the expression array holds.
    pub fn word_count(&self) -> u32 {
        self.words
    }

    /// This partition's bytes, exactly as they would be written — so the durable form and the
    /// in-memory form cannot be produced by two different encoders.
    pub fn as_bytes(&self) -> &[u8] {
        self.bytes.as_slice()
    }

    fn u32_at(&self, at: usize) -> u32 {
        let raw = self.bytes.as_slice();
        u32::from_le_bytes([raw[at], raw[at + 1], raw[at + 2], raw[at + 3]])
    }
}

// A third format, in this module for [`ContainmentPack`]'s reason — a per-generation artifact
// structure written into the prefix at the fold, addressed by dense ordinal, and served **mapped**
// (the layout memo's constraint 13). Its own magic and its own version, so a reader handed one of
// the three where another belongs refuses instead of finding a plausible header.
//
// ```text
// header := magic "TSTI" | u16 version | u16 reserved | u32 ordinals | u32 row_count
// spans  := u32 LE min, u32 LE max, per ordinal
// ```
//
// **What this file holds is the extents, and the node hierarchy above them is derived at open.**
// `design/artifact-serving-at-scale.md` §3 sizes the two halves apart: 80 MB of extents at 10⁷
// artifacts against 4.2 MB of index. The extents are the half that scales with the population and
// the half worth mapping; the tree is a pure function of them — an artifact's node is the finest
// whose block holds both ends of its span — so writing it would be writing a derivation of the
// bytes beside it, and a second thing that could disagree with them. `tessera_engine::tile_index`
// folds it up in one pass over this column, which is the same pass that validates the column.
//
// # The two sentinels, and why they are not one
//
// A span is `min <= max` where the artifact has rows. Where it has none there are **two** states
// and they are different facts (the layout memo's constraint 3):
//
// - [`TILE_INDEX_HOLE`] — no artifact at this ordinal. A fold executed a deletion and the slot is
//   held open because an ordinal is identity.
// - [`TILE_INDEX_EMPTY`] — a live artifact whose membership projects to nothing in this view: every
//   member is awaiting a fold. It is absent from every viewport and **still an artifact**, served
//   by the identifier route exactly as it was.
//
// Neither is ever a candidate, so collapsing them costs nothing *today* — and that is exactly why
// the distinction has to be in the format rather than in a comment: a reader that could not tell
// them apart would have no way to answer "is this ordinal an artifact" from the column, and the
// first caller that needed to would get the permissive answer for a hole.

const TILE_INDEX_MAGIC: &[u8; 4] = b"TSTI";
/// Bumped whenever the header or the span encoding changes. There is no compatibility to keep
/// (decision 0048); the number exists so a stale local file is a loud refusal rather than a silent
/// misread of which artifacts a viewport reaches.
const TILE_INDEX_VERSION: u16 = 1;
const TILE_INDEX_HEADER_LEN: usize = 4 + 2 + 2 + 4 + 4;

/// The span of an ordinal that holds no artifact at all. See the module's note on the two
/// sentinels.
pub const TILE_INDEX_HOLE: (u32, u32) = (u32::MAX, 0);
/// The span of a live artifact whose membership projects to nothing in this view.
pub const TILE_INDEX_EMPTY: (u32, u32) = (u32::MAX, 1);

fn tile_index_malformed(what: &str, detail: impl std::fmt::Display) -> StoreError {
    StoreError::MalformedBundle {
        detail: format!("tile index {what}: {detail}"),
    }
}

/// Serialise one `(view, layer, level)`'s per-artifact extents.
///
/// `row_count` is the view's row space at composition; the header carries it so the hierarchy the
/// reader folds up has the same shape the writer's did, and so a span reaching past it is a
/// refusal rather than a node nobody walks.
///
/// Returns the bytes rather than writing them, for [`pack`]'s reason: the caller owns the
/// durability sequence.
pub fn pack_tile_index(row_count: u32, spans: &[(u32, u32)]) -> Vec<u8> {
    // Never below the highest row a span names, so the framing check below is a check on the file
    // rather than on the row space that produced it — a caller passing a base row count that a
    // projection has legitimately reached is not a malformed bundle.
    let row_count = spans
        .iter()
        .filter(|(lo, hi)| lo <= hi)
        .map(|(_, hi)| hi.saturating_add(1))
        .max()
        .unwrap_or(0)
        .max(row_count);
    let mut out = Vec::with_capacity(TILE_INDEX_HEADER_LEN + spans.len() * 8);
    out.extend_from_slice(TILE_INDEX_MAGIC);
    out.extend_from_slice(&TILE_INDEX_VERSION.to_le_bytes());
    out.extend_from_slice(&0u16.to_le_bytes());
    out.extend_from_slice(&(spans.len() as u32).to_le_bytes());
    out.extend_from_slice(&row_count.to_le_bytes());
    for (lo, hi) in spans {
        out.extend_from_slice(&lo.to_le_bytes());
        out.extend_from_slice(&hi.to_le_bytes());
    }
    out
}

/// One `(view, layer, level)`'s per-artifact extents, framed and checked once at open.
///
/// **Mapped, not decoded** — [`ContainmentPack`]'s argument, and here the figure is larger: 80 MB
/// per level at ten million artifacts, against 4.2 MB for the hierarchy folded over it.
pub struct TileIndexPack {
    bytes: ContainmentBytes,
    ordinals: u32,
    row_count: u32,
}

impl std::fmt::Debug for TileIndexPack {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("TileIndexPack")
            .field("ordinals", &self.ordinals)
            .field("row_count", &self.row_count)
            .finish()
    }
}

impl TileIndexPack {
    /// Open a fold-written index, mapped in place.
    pub fn open(path: &Path) -> Result<Self> {
        let file = File::open(path).map_err(|source| StoreError::Io {
            path: path.to_path_buf(),
            source,
        })?;
        // SAFETY: as [`MembershipPack::open`] — read-only, and nothing truncates a published
        // prefix's files while a generation names them.
        let map = unsafe { Mmap::map(&file) }.map_err(|source| StoreError::Io {
            path: path.to_path_buf(),
            source,
        })?;
        Self::frame(ContainmentBytes::Mapped(map), &path.display().to_string())
    }

    /// The same structure held in memory, checked by the same rules — the form a publication
    /// builds before any fold has consolidated it.
    pub fn from_bytes(bytes: Vec<u8>) -> Result<Self> {
        Self::frame(ContainmentBytes::Owned(bytes), "(in memory)")
    }

    /// Every check refuses rather than truncating, and the direction of the mistake is what makes
    /// that the only option: a column read short leaves every ordinal past the truncation absent
    /// from the walk, so the artifacts simply stop being served — indistinguishable from artifacts
    /// that failed an existence criterion, with nothing downstream to notice. A span read *wide*
    /// is the other direction and costs a probe, not an answer; both are refused here because a
    /// file that can be wrong in one direction is not trustworthy in the other.
    fn frame(bytes: ContainmentBytes, what: &str) -> Result<Self> {
        let raw = bytes.as_slice();
        if raw.len() < TILE_INDEX_HEADER_LEN {
            return Err(tile_index_malformed(
                what,
                format!(
                    "{} bytes is shorter than the {TILE_INDEX_HEADER_LEN}-byte header",
                    raw.len()
                ),
            ));
        }
        if &raw[0..4] != TILE_INDEX_MAGIC {
            return Err(tile_index_malformed(what, "magic is not TSTI"));
        }
        let version = u16::from_le_bytes([raw[4], raw[5]]);
        if version != TILE_INDEX_VERSION {
            return Err(tile_index_malformed(
                what,
                format!("version {version}, expected {TILE_INDEX_VERSION}"),
            ));
        }
        let reserved = u16::from_le_bytes([raw[6], raw[7]]);
        if reserved != 0 {
            return Err(tile_index_malformed(
                what,
                format!("reserved is {reserved}, expected 0"),
            ));
        }
        let read = |at: usize| u32::from_le_bytes([raw[at], raw[at + 1], raw[at + 2], raw[at + 3]]);
        let ordinals = read(8);
        let row_count = read(12);
        let total = TILE_INDEX_HEADER_LEN + ordinals as usize * 8;
        if raw.len() != total {
            return Err(tile_index_malformed(
                what,
                format!(
                    "{} bytes, but the header describes {total} — the file is truncated or was \
                     written by a different packer",
                    raw.len()
                ),
            ));
        }
        let pack = TileIndexPack {
            bytes,
            ordinals,
            row_count,
        };
        // One pass over the column, which is the pass the caller is about to make anyway to fold
        // the hierarchy up. A span that is neither a sentinel nor a range inside the row space is
        // a file this packer did not write.
        for ordinal in 0..ordinals as usize {
            let span = pack.span(ordinal);
            if span == TILE_INDEX_HOLE || span == TILE_INDEX_EMPTY {
                continue;
            }
            let (lo, hi) = span;
            if lo > hi {
                return Err(tile_index_malformed(
                    what,
                    format!("the span at ordinal {ordinal} is {lo}..={hi}, which is neither a range nor a sentinel"),
                ));
            }
            if hi >= row_count {
                return Err(tile_index_malformed(
                    what,
                    format!(
                        "the span at ordinal {ordinal} reaches row {hi} of a {row_count}-row space"
                    ),
                ));
            }
        }
        Ok(pack)
    }

    /// How many ordinals this index covers, holes included.
    pub fn ordinals(&self) -> u32 {
        self.ordinals
    }

    /// The row space this index was folded over.
    pub fn row_count(&self) -> u32 {
        self.row_count
    }

    /// The `(min_row, max_row)` at `ordinal`, or one of the two sentinels. Infallible for
    /// `ordinal < ordinals()`, which [`Self::frame`] proved.
    pub fn span(&self, ordinal: usize) -> (u32, u32) {
        let at = TILE_INDEX_HEADER_LEN + ordinal * 8;
        (self.u32_at(at), self.u32_at(at + 4))
    }

    /// This index's bytes, exactly as they would be written — so the durable form and the
    /// in-memory form cannot be produced by two different encoders.
    pub fn as_bytes(&self) -> &[u8] {
        self.bytes.as_slice()
    }

    fn u32_at(&self, at: usize) -> u32 {
        let raw = self.bytes.as_slice();
        u32::from_le_bytes([raw[at], raw[at + 1], raw[at + 2], raw[at + 3]])
    }
}

// ---- the row-major columns ------------------------------------------------------------------
//
// Two more formats, in this module for [`ContainmentPack`]'s reason — per-generation artifact
// structures written into the prefix at the fold, addressed by **row** rather than by ordinal, and
// served mapped (the layout memo's constraint 13). Each gets its own magic and its own version, so
// a reader handed one of the five where another belongs refuses instead of finding a plausible
// header.
//
// ```text
// label := magic "TSLB" | u16 version | u8 width | u8 reserved | u32 rows | u32 ordinals
//          labels: width bytes x rows
//
// list  := magic "TSLL" | u16 version | u8 width | u8 reserved | u32 rows | u32 ordinals
//                       | u32 entries
//          at:     u32 LE x (rows + 1)   -- into `values`; at[0] == 0, ascending, at[rows] == entries
//          values: width bytes x entries
// ```
//
// # The width, and the sentinel that costs one value
//
// The label width is `u8`, `u16` or `u32`, chosen by the artifact count — `configuration.md` §1's
// declared widths, applied to a column of ordinals rather than of values. **The all-ones value at
// each width is [`ROW_COLUMN_HOLE`]**: a row belonging to no artifact, which is most rows of most
// layers and is a state the format has to be able to say. So a width admits `2^bits − 1` ordinals
// and not `2^bits`, and a level at exactly the boundary takes the next width up. That is a value
// per column rather than a bit per row, which is the right side of the trade at four bytes a row.
//
// A **list** column needs no such sentinel: a row belonging to no artifact has an empty list, which
// the offsets already express. Its width bounds the same thing for the same reason.
//
// # Fail-closed
//
// Bad magic, an unknown version, an unknown width, a length the header does not describe,
// non-ascending offsets, or a label naming an ordinal the level does not hold all **refuse**. The
// direction of the mistake is the tile index's: a column read short leaves every row past the
// truncation belonging to nobody, so the artifacts holding those rows simply stop being candidates
// — indistinguishable from artifacts that failed an existence criterion, with nothing downstream to
// notice. A label read *wide* is worse still, since it would count one artifact's rows under
// another's identity.

/// The label of a row no artifact claims, at whatever width the column is in: **all ones**.
/// Returned by [`LabelColumnPack::label`] as `u32::MAX` whatever the stored width, so a caller
/// compares against one constant rather than against three.
pub const ROW_COLUMN_HOLE: u32 = u32::MAX;

const LABEL_MAGIC: &[u8; 4] = b"TSLB";
const LIST_MAGIC: &[u8; 4] = b"TSLL";
/// Bumped whenever the header or the label encoding changes. There is no compatibility to keep
/// (decision 0048); the number exists so a stale local file is a loud refusal rather than a silent
/// misread of which artifact a row belongs to.
const ROW_COLUMN_VERSION: u16 = 1;
const LABEL_HEADER_LEN: usize = 4 + 2 + 1 + 1 + 4 + 4;
const LIST_HEADER_LEN: usize = 4 + 2 + 1 + 1 + 4 + 4 + 4;

fn row_column_malformed(what: &str, detail: impl std::fmt::Display) -> StoreError {
    StoreError::MalformedBundle {
        detail: format!("row-major column {what}: {detail}"),
    }
}

/// The narrowest width that can address `ordinals` artifacts **and** the hole sentinel.
///
/// `configuration.md` §1's `u8`/`u16`/`u32` discipline, applied to ordinals. One fewer value than
/// the width holds, because the all-ones value is [`ROW_COLUMN_HOLE`].
pub fn row_column_width(ordinals: u64) -> u8 {
    if ordinals < u64::from(u8::MAX) {
        1
    } else if ordinals < u64::from(u16::MAX) {
        2
    } else {
        4
    }
}

/// The hole sentinel as it is stored at `width`.
fn hole_at(width: u8) -> u32 {
    match width {
        1 => u32::from(u8::MAX),
        2 => u32::from(u16::MAX),
        _ => u32::MAX,
    }
}

fn put_narrow(out: &mut Vec<u8>, width: u8, value: u32) {
    match width {
        1 => out.push(value as u8),
        2 => out.extend_from_slice(&(value as u16).to_le_bytes()),
        _ => out.extend_from_slice(&value.to_le_bytes()),
    }
}

/// Serialise one `(view, layer, level)`'s label column: `labels[row]` is the ordinal that row
/// belongs to, or [`ROW_COLUMN_HOLE`].
///
/// `ordinals` is how many ordinals the level holds, holes included — carried so a reader can refuse
/// a label naming an artifact the level does not have, which is what a truncated identifier column
/// would otherwise look like.
///
/// Returns the bytes rather than writing them, for [`pack`]'s reason: the caller owns the durability
/// sequence.
pub fn pack_label_column(ordinals: u32, labels: &[u32]) -> Vec<u8> {
    let width = row_column_width(u64::from(ordinals));
    let hole = hole_at(width);
    let mut out = Vec::with_capacity(LABEL_HEADER_LEN + labels.len() * usize::from(width));
    out.extend_from_slice(LABEL_MAGIC);
    out.extend_from_slice(&ROW_COLUMN_VERSION.to_le_bytes());
    out.push(width);
    out.push(0);
    out.extend_from_slice(&(labels.len() as u32).to_le_bytes());
    out.extend_from_slice(&ordinals.to_le_bytes());
    for label in labels {
        put_narrow(
            &mut out,
            width,
            if *label == ROW_COLUMN_HOLE {
                hole
            } else {
                *label
            },
        );
    }
    out
}

/// Serialise one `(view, layer, level)`'s list column: `at[row]..at[row + 1]` into `values`.
pub fn pack_list_column(ordinals: u32, at: &[u32], values: &[u32]) -> Vec<u8> {
    let width = row_column_width(u64::from(ordinals));
    let rows = at.len().saturating_sub(1) as u32;
    let mut out =
        Vec::with_capacity(LIST_HEADER_LEN + at.len() * 4 + values.len() * usize::from(width));
    out.extend_from_slice(LIST_MAGIC);
    out.extend_from_slice(&ROW_COLUMN_VERSION.to_le_bytes());
    out.push(width);
    out.push(0);
    out.extend_from_slice(&rows.to_le_bytes());
    out.extend_from_slice(&ordinals.to_le_bytes());
    out.extend_from_slice(&(values.len() as u32).to_le_bytes());
    for offset in at {
        out.extend_from_slice(&offset.to_le_bytes());
    }
    for value in values {
        put_narrow(&mut out, width, *value);
    }
    out
}

/// One `(view, layer, level)`'s label column, framed and checked once at open.
///
/// **Mapped, not decoded** — [`ContainmentPack`]'s argument, and here it is the whole point of the
/// layout: the row-major form is one narrow integer per row *whatever the artifact count*, and it
/// exists because at 10⁹ rows the artifact-major form does not fit at all
/// (`design/artifact-serving-at-scale.md` §5.1).
pub struct LabelColumnPack {
    bytes: ContainmentBytes,
    width: u8,
    rows: u32,
    ordinals: u32,
}

impl std::fmt::Debug for LabelColumnPack {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("LabelColumnPack")
            .field("rows", &self.rows)
            .field("ordinals", &self.ordinals)
            .field("width", &self.width)
            .finish()
    }
}

impl LabelColumnPack {
    /// Open a fold-written label column, mapped in place.
    pub fn open(path: &Path) -> Result<Self> {
        // SAFETY: as [`MembershipPack::open`] — read-only, and nothing truncates a published
        // prefix's files while a generation names them.
        let map = map_read_only(path)?;
        Self::frame(ContainmentBytes::Mapped(map), &path.display().to_string())
    }

    /// The same structure held in memory, checked by the same rules — the form a publication builds
    /// before any fold has consolidated it.
    pub fn from_bytes(bytes: Vec<u8>) -> Result<Self> {
        Self::frame(ContainmentBytes::Owned(bytes), "(in memory)")
    }

    fn frame(bytes: ContainmentBytes, what: &str) -> Result<Self> {
        let raw = bytes.as_slice();
        if raw.len() < LABEL_HEADER_LEN {
            return Err(row_column_malformed(
                what,
                format!(
                    "{} bytes is shorter than the {LABEL_HEADER_LEN}-byte header",
                    raw.len()
                ),
            ));
        }
        if &raw[0..4] != LABEL_MAGIC {
            return Err(row_column_malformed(what, "magic is not TSLB"));
        }
        let version = u16::from_le_bytes([raw[4], raw[5]]);
        if version != ROW_COLUMN_VERSION {
            return Err(row_column_malformed(
                what,
                format!("version {version}, expected {ROW_COLUMN_VERSION}"),
            ));
        }
        let width = raw[6];
        if !matches!(width, 1 | 2 | 4) {
            return Err(row_column_malformed(
                what,
                format!("label width {width}, expected 1, 2 or 4"),
            ));
        }
        if raw[7] != 0 {
            return Err(row_column_malformed(
                what,
                format!("reserved is {}, expected 0", raw[7]),
            ));
        }
        let read = |at: usize| u32::from_le_bytes([raw[at], raw[at + 1], raw[at + 2], raw[at + 3]]);
        let rows = read(8);
        let ordinals = read(12);
        // **A narrow column cannot address a wide level**, and the check is one value tighter than
        // the obvious one because the all-ones value is the hole.
        if u64::from(ordinals) >= u64::from(hole_at(width)) {
            return Err(row_column_malformed(
                what,
                format!("{ordinals} ordinals cannot be addressed at width {width}"),
            ));
        }
        let total = LABEL_HEADER_LEN + rows as usize * usize::from(width);
        if raw.len() != total {
            return Err(row_column_malformed(
                what,
                format!(
                    "{} bytes, but the header describes {total} — the file is truncated or was \
                     written by a different packer",
                    raw.len()
                ),
            ));
        }
        let pack = LabelColumnPack {
            bytes,
            width,
            rows,
            ordinals,
        };
        // One pass over the column, which is the pass the caller is about to make anyway to derive
        // the declared sizes. A label naming an ordinal the level does not hold is what a truncated
        // or foreign column looks like, and serving it would count one artifact's rows under
        // another's identity.
        for row in 0..rows as usize {
            let label = pack.label(row);
            if label != ROW_COLUMN_HOLE && label >= ordinals {
                return Err(row_column_malformed(
                    what,
                    format!("row {row} is labelled {label} of {ordinals} ordinals"),
                ));
            }
        }
        Ok(pack)
    }

    /// How many rows this column covers.
    pub fn rows(&self) -> u32 {
        self.rows
    }

    /// How many ordinals the level holds, holes included.
    pub fn ordinals(&self) -> u32 {
        self.ordinals
    }

    /// Bytes per label — 1, 2 or 4.
    pub fn width(&self) -> u8 {
        self.width
    }

    /// The ordinal `row` belongs to, or [`ROW_COLUMN_HOLE`]. Infallible for `row < rows()`, which
    /// [`Self::frame`] proved; a row past the column is a hole.
    pub fn label(&self, row: usize) -> u32 {
        if row >= self.rows as usize {
            return ROW_COLUMN_HOLE;
        }
        let raw = self.bytes.as_slice();
        let at = LABEL_HEADER_LEN + row * usize::from(self.width);
        let value = match self.width {
            1 => u32::from(raw[at]),
            2 => u32::from(u16::from_le_bytes([raw[at], raw[at + 1]])),
            _ => u32::from_le_bytes([raw[at], raw[at + 1], raw[at + 2], raw[at + 3]]),
        };
        if value == hole_at(self.width) {
            ROW_COLUMN_HOLE
        } else {
            value
        }
    }

    /// This column's bytes, exactly as they would be written — so the durable form and the
    /// in-memory form cannot be produced by two different encoders.
    pub fn as_bytes(&self) -> &[u8] {
        self.bytes.as_slice()
    }
}

/// One `(view, layer, level)`'s list column, framed and checked once at open. The label column's
/// format where a row may belong to several artifacts — the same inversion at a larger constant
/// (`design/artifact-serving-at-scale.md` §5.1).
pub struct ListColumnPack {
    bytes: ContainmentBytes,
    width: u8,
    rows: u32,
    ordinals: u32,
    entries: u32,
    values_at: usize,
}

impl std::fmt::Debug for ListColumnPack {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ListColumnPack")
            .field("rows", &self.rows)
            .field("ordinals", &self.ordinals)
            .field("entries", &self.entries)
            .field("width", &self.width)
            .finish()
    }
}

impl ListColumnPack {
    /// Open a fold-written list column, mapped in place.
    pub fn open(path: &Path) -> Result<Self> {
        // SAFETY: as [`MembershipPack::open`].
        let map = map_read_only(path)?;
        Self::frame(ContainmentBytes::Mapped(map), &path.display().to_string())
    }

    /// The same structure held in memory, checked by the same rules.
    pub fn from_bytes(bytes: Vec<u8>) -> Result<Self> {
        Self::frame(ContainmentBytes::Owned(bytes), "(in memory)")
    }

    fn frame(bytes: ContainmentBytes, what: &str) -> Result<Self> {
        let raw = bytes.as_slice();
        if raw.len() < LIST_HEADER_LEN {
            return Err(row_column_malformed(
                what,
                format!(
                    "{} bytes is shorter than the {LIST_HEADER_LEN}-byte header",
                    raw.len()
                ),
            ));
        }
        if &raw[0..4] != LIST_MAGIC {
            return Err(row_column_malformed(what, "magic is not TSLL"));
        }
        let version = u16::from_le_bytes([raw[4], raw[5]]);
        if version != ROW_COLUMN_VERSION {
            return Err(row_column_malformed(
                what,
                format!("version {version}, expected {ROW_COLUMN_VERSION}"),
            ));
        }
        let width = raw[6];
        if !matches!(width, 1 | 2 | 4) {
            return Err(row_column_malformed(
                what,
                format!("entry width {width}, expected 1, 2 or 4"),
            ));
        }
        if raw[7] != 0 {
            return Err(row_column_malformed(
                what,
                format!("reserved is {}, expected 0", raw[7]),
            ));
        }
        let read = |at: usize| u32::from_le_bytes([raw[at], raw[at + 1], raw[at + 2], raw[at + 3]]);
        let rows = read(8);
        let ordinals = read(12);
        let entries = read(16);
        if u64::from(ordinals) >= u64::from(hole_at(width)) {
            return Err(row_column_malformed(
                what,
                format!("{ordinals} ordinals cannot be addressed at width {width}"),
            ));
        }
        let values_at = LIST_HEADER_LEN + (rows as usize + 1) * 4;
        let total = values_at + entries as usize * usize::from(width);
        if raw.len() != total {
            return Err(row_column_malformed(
                what,
                format!(
                    "{} bytes, but the header describes {total} — the file is truncated or was \
                     written by a different packer",
                    raw.len()
                ),
            ));
        }
        let pack = ListColumnPack {
            bytes,
            width,
            rows,
            ordinals,
            entries,
            values_at,
        };
        if pack.at(0) != 0 || pack.at(rows as usize) != entries {
            return Err(row_column_malformed(
                what,
                "the row offsets do not span exactly the value column",
            ));
        }
        for row in 0..rows as usize {
            if pack.at(row) > pack.at(row + 1) {
                return Err(row_column_malformed(
                    what,
                    format!("row offset {row} goes backwards"),
                ));
            }
        }
        for i in 0..entries as usize {
            if pack.value(i) >= ordinals {
                return Err(row_column_malformed(
                    what,
                    format!("entry {i} names ordinal {} of {ordinals}", pack.value(i)),
                ));
            }
        }
        Ok(pack)
    }

    pub fn rows(&self) -> u32 {
        self.rows
    }

    pub fn ordinals(&self) -> u32 {
        self.ordinals
    }

    pub fn entries(&self) -> u32 {
        self.entries
    }

    pub fn width(&self) -> u8 {
        self.width
    }

    /// Where `row`'s list begins in the value column. Infallible for `row <= rows()`.
    pub fn at(&self, row: usize) -> u32 {
        let raw = self.bytes.as_slice();
        let at = LIST_HEADER_LEN + row * 4;
        u32::from_le_bytes([raw[at], raw[at + 1], raw[at + 2], raw[at + 3]])
    }

    /// One entry of the value column.
    pub fn value(&self, index: usize) -> u32 {
        let raw = self.bytes.as_slice();
        let at = self.values_at + index * usize::from(self.width);
        match self.width {
            1 => u32::from(raw[at]),
            2 => u32::from(u16::from_le_bytes([raw[at], raw[at + 1]])),
            _ => u32::from_le_bytes([raw[at], raw[at + 1], raw[at + 2], raw[at + 3]]),
        }
    }

    /// The ordinals `row` belongs to. Empty for a row no artifact claims, and for a row past the
    /// column.
    pub fn list(&self, row: usize) -> impl Iterator<Item = u32> + '_ {
        let (lo, hi) = if row < self.rows as usize {
            (self.at(row) as usize, self.at(row + 1) as usize)
        } else {
            (0, 0)
        };
        (lo..hi).map(move |i| self.value(i))
    }

    /// This column's bytes, exactly as they would be written.
    pub fn as_bytes(&self) -> &[u8] {
        self.bytes.as_slice()
    }
}

// ---------------------------------------------------------------------------------------------
// The shape row form: one segment's resolved membership of one spatial level
// (`polygon-membership.md` §6.3), persisted so an open claims it rather than resolving again.
// ---------------------------------------------------------------------------------------------

const SHAPE_ROWS_MAGIC: &[u8; 4] = b"TSSR";
const SHAPE_ROWS_VERSION: u16 = 1;
/// Magic, version, reserved, ordinals, row count, level version, segment-id length.
const SHAPE_ROWS_HEADER_LEN: usize = 4 + 2 + 2 + 4 + 4 + 8 + 2;
/// The per-ordinal length that marks a hole — no artifact at that ordinal, as distinct from an
/// artifact whose membership of this segment is empty, which is a zero-length entry.
const SHAPE_ROWS_HOLE: u32 = u32::MAX;

fn shape_rows_malformed(what: &str, detail: impl std::fmt::Display) -> StoreError {
    StoreError::MalformedBundle {
        detail: format!("shape rows {what}: {detail}"),
    }
}

/// Serialise one `(view, layer, level)`'s membership of **one segment**, one entry per ordinal.
///
/// ```text
/// TSSR | u16 version | u16 reserved (0) | u32 ordinals | u32 row_count | u64 level_version
///      | u16 seg_id_len | seg_id bytes
///      | per ordinal: u32 len — u32::MAX a hole, 0 an empty membership — then `len` bytes,
///        a portable Roaring bitmap of segment-local rows
/// ```
///
/// **The key is in the file as well as in the manifest entry that names it**, because the two are
/// what a reader refuses on: a piece is a function of the segment's rows and the level's shapes at
/// one level version, and one written against any other segment or version names rows that mean
/// something else. `row_count` is the segment's, and a bitmap naming a row at or past it is refused
/// at read rather than trusted.
///
/// `entries` is per ordinal: `None` a hole, `Some(bytes)` the bitmap's portable serialisation —
/// produced by `derived::shape_rows_bytes`, which owns the bitmap side.
pub fn pack_shape_rows(
    level_version: u64,
    seg_id: &str,
    row_count: u32,
    entries: &[Option<Vec<u8>>],
) -> Vec<u8> {
    let payload: usize = entries.iter().map(|e| 4 + e.as_ref().map_or(0, Vec::len)).sum();
    let mut out = Vec::with_capacity(SHAPE_ROWS_HEADER_LEN + seg_id.len() + payload);
    out.extend_from_slice(SHAPE_ROWS_MAGIC);
    out.extend_from_slice(&SHAPE_ROWS_VERSION.to_le_bytes());
    out.extend_from_slice(&0u16.to_le_bytes());
    out.extend_from_slice(&(entries.len() as u32).to_le_bytes());
    out.extend_from_slice(&row_count.to_le_bytes());
    out.extend_from_slice(&level_version.to_le_bytes());
    out.extend_from_slice(&(seg_id.len() as u16).to_le_bytes());
    out.extend_from_slice(seg_id.as_bytes());
    for entry in entries {
        match entry {
            None => out.extend_from_slice(&SHAPE_ROWS_HOLE.to_le_bytes()),
            Some(bytes) => {
                out.extend_from_slice(&(bytes.len() as u32).to_le_bytes());
                out.extend_from_slice(bytes);
            }
        }
    }
    out
}

/// One shape row form, framed and checked once at open: the header's key and each entry's extent
/// are verified before any bitmap is decoded.
pub struct ShapeRowsPack {
    bytes: ContainmentBytes,
    ordinals: u32,
    row_count: u32,
    level_version: u64,
    seg_id: String,
    /// Byte offset of each ordinal's length word, so an entry is addressed without a second walk.
    at: Vec<usize>,
}

impl std::fmt::Debug for ShapeRowsPack {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ShapeRowsPack")
            .field("ordinals", &self.ordinals)
            .field("row_count", &self.row_count)
            .field("level_version", &self.level_version)
            .field("seg_id", &self.seg_id)
            .finish()
    }
}

impl ShapeRowsPack {
    /// Open a written row form, mapped in place.
    pub fn open(path: &Path) -> Result<Self> {
        let map = map_read_only(path)?;
        Self::frame(ContainmentBytes::Mapped(map), &path.display().to_string())
    }

    pub fn from_bytes(bytes: Vec<u8>) -> Result<Self> {
        Self::frame(ContainmentBytes::Owned(bytes), "(in memory)")
    }

    /// Every check refuses rather than truncating, on `TileIndexPack::frame`'s argument: a form
    /// read short leaves every ordinal past the truncation with no rows, which is a masked count
    /// that silently understates for every viewer.
    fn frame(bytes: ContainmentBytes, what: &str) -> Result<Self> {
        let raw = bytes.as_slice();
        if raw.len() < SHAPE_ROWS_HEADER_LEN {
            return Err(shape_rows_malformed(
                what,
                format!(
                    "{} bytes is shorter than the {SHAPE_ROWS_HEADER_LEN}-byte header",
                    raw.len()
                ),
            ));
        }
        if &raw[0..4] != SHAPE_ROWS_MAGIC {
            return Err(shape_rows_malformed(what, "magic is not TSSR"));
        }
        let version = u16::from_le_bytes([raw[4], raw[5]]);
        if version != SHAPE_ROWS_VERSION {
            return Err(shape_rows_malformed(
                what,
                format!("version {version}, expected {SHAPE_ROWS_VERSION}"),
            ));
        }
        let reserved = u16::from_le_bytes([raw[6], raw[7]]);
        if reserved != 0 {
            return Err(shape_rows_malformed(
                what,
                format!("reserved is {reserved}, expected 0"),
            ));
        }
        let read = |at: usize| u32::from_le_bytes([raw[at], raw[at + 1], raw[at + 2], raw[at + 3]]);
        let ordinals = read(8);
        let row_count = read(12);
        let level_version = u64::from_le_bytes(raw[16..24].try_into().expect("eight bytes"));
        let seg_id_len = usize::from(u16::from_le_bytes([raw[24], raw[25]]));
        let mut cursor = SHAPE_ROWS_HEADER_LEN;
        if raw.len() < cursor + seg_id_len {
            return Err(shape_rows_malformed(what, "truncated inside the segment id"));
        }
        let seg_id = std::str::from_utf8(&raw[cursor..cursor + seg_id_len])
            .map_err(|_| shape_rows_malformed(what, "the segment id is not UTF-8"))?
            .to_string();
        cursor += seg_id_len;
        let mut at = Vec::with_capacity(ordinals as usize);
        for ordinal in 0..ordinals {
            if raw.len() < cursor + 4 {
                return Err(shape_rows_malformed(
                    what,
                    format!("truncated at ordinal {ordinal} of {ordinals}"),
                ));
            }
            at.push(cursor);
            let len = read(cursor);
            cursor += 4;
            if len != SHAPE_ROWS_HOLE {
                let len = len as usize;
                if raw.len() < cursor + len {
                    return Err(shape_rows_malformed(
                        what,
                        format!("truncated inside ordinal {ordinal}'s bitmap"),
                    ));
                }
                cursor += len;
            }
        }
        if cursor != raw.len() {
            return Err(shape_rows_malformed(
                what,
                format!(
                    "{} bytes, but the entries end at {cursor} — trailing bytes the packer did \
                     not write",
                    raw.len()
                ),
            ));
        }
        Ok(ShapeRowsPack {
            bytes,
            ordinals,
            row_count,
            level_version,
            seg_id,
            at,
        })
    }

    pub fn ordinals(&self) -> u32 {
        self.ordinals
    }

    /// The segment's row count the form was resolved over — every row a bitmap names is below it.
    pub fn row_count(&self) -> u32 {
        self.row_count
    }

    pub fn level_version(&self) -> u64 {
        self.level_version
    }

    pub fn seg_id(&self) -> &str {
        &self.seg_id
    }

    /// One ordinal's entry: `None` at a hole, `Some(&[])` for an empty membership, otherwise the
    /// bitmap's portable bytes.
    pub fn entry(&self, ordinal: u32) -> Option<&[u8]> {
        let raw = self.bytes.as_slice();
        let at = *self.at.get(ordinal as usize)?;
        let len = u32::from_le_bytes([raw[at], raw[at + 1], raw[at + 2], raw[at + 3]]);
        if len == SHAPE_ROWS_HOLE {
            return None;
        }
        Some(&raw[at + 4..at + 4 + len as usize])
    }
}

/// The read-only mapping the two row-major columns open with — one function rather than a third
/// and fourth copy of the same safety argument: the file is opened read-only and the mapping is
/// never written through, and nothing truncates a published prefix's files while a generation names
/// them.
fn map_read_only(path: &Path) -> Result<Mmap> {
    let file = File::open(path).map_err(|source| StoreError::Io {
        path: path.to_path_buf(),
        source,
    })?;
    // SAFETY: see this function's doc.
    unsafe { Mmap::map(&file) }.map_err(|source| StoreError::Io {
        path: path.to_path_buf(),
        source,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn write(dir: &Path, name: &str, bytes: &[u8]) -> std::path::PathBuf {
        let path = dir.join(name);
        std::fs::write(&path, bytes).unwrap();
        path
    }

    #[test]
    fn an_extent_round_trips_and_addresses_by_absolute_ordinal() {
        let tmp = tempfile::tempdir().unwrap();
        let blobs = vec![vec![1u8, 2, 3], Vec::new(), vec![9u8; 300]];
        let path = write(tmp.path(), "m.tsmb", &pack(1000, &blobs));

        let pack = MembershipPack::open(&path).unwrap();
        assert_eq!(pack.ordinal_lo(), 1000);
        assert_eq!(pack.count(), 3);

        // Absolute ordinals, not indices — addressing the second extent from zero is the mistake
        // this signature exists to prevent.
        assert_eq!(pack.blob(1000), Some(&[1u8, 2, 3][..]));
        // An empty membership is a real state (every member deleted) and is not a hole.
        assert_eq!(pack.blob(1001), Some(&[][..]));
        assert_eq!(pack.blob(1002).map(<[u8]>::len), Some(300));
        assert_eq!(pack.blob(999), None);
        assert_eq!(pack.blob(1003), None);

        let all: Vec<(u32, usize)> = pack.iter().map(|(o, b)| (o, b.len())).collect();
        assert_eq!(all, vec![(1000, 3), (1001, 0), (1002, 300)]);
    }

    /// **The streaming writer's bytes are [`pack`]'s bytes**, which is the whole of its contract:
    /// a build writes its extents through it and a fold writes them through `pack`, and a reader
    /// cannot tell which produced the file it opened.
    #[test]
    fn the_streaming_writer_produces_the_same_bytes_as_pack() {
        let tmp = tempfile::tempdir().unwrap();
        let blobs = vec![vec![1u8, 2, 3], Vec::new(), vec![9u8; 300], vec![4u8; 7]];
        let path = tmp.path().join("streamed.tsmb");
        let mut writer = PackWriter::create(&path, 1000, blobs.len() as u32).unwrap();
        for blob in &blobs {
            writer.push(blob).unwrap();
        }
        writer.finish().unwrap();
        assert_eq!(std::fs::read(&path).unwrap(), pack(1000, &blobs));

        let opened = MembershipPack::open(&path).unwrap();
        assert_eq!(opened.ordinal_lo(), 1000);
        assert_eq!(opened.blob(1002).map(<[u8]>::len), Some(300));
    }

    /// **A count that is not kept refuses.** The count is the level's ordinal range, so an extent
    /// short of it addresses artifacts whose membership decodes as absent — which the existence
    /// criterion then renders as a cluster that failed its bar, with nothing to notice.
    #[test]
    fn a_streamed_extent_that_does_not_fill_its_range_refuses() {
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("short.tsmb");
        let mut writer = PackWriter::create(&path, 0, 3).unwrap();
        writer.push(&[1u8, 2]).unwrap();
        writer.push(&[3u8]).unwrap();
        assert!(writer.finish().is_err());

        let path = tmp.path().join("long.tsmb");
        let mut writer = PackWriter::create(&path, 0, 2).unwrap();
        writer.push(&[1u8]).unwrap();
        writer.push(&[2u8]).unwrap();
        assert!(writer.push(&[3u8]).is_err());
    }

    #[test]
    fn an_empty_extent_is_well_formed() {
        let tmp = tempfile::tempdir().unwrap();
        let path = write(tmp.path(), "m.tsmb", &pack(0, &[]));
        let pack = MembershipPack::open(&path).unwrap();
        assert_eq!(pack.count(), 0);
        assert_eq!(pack.iter().count(), 0);
        assert_eq!(pack.blob(0), None);
    }

    /// **The assertion that matters.** A truncated extent must refuse, not decode to a smaller one:
    /// a membership that came back short is an artifact with a low masked count for every viewer,
    /// which the existence criterion renders as *absent* — indistinguishable from a cluster that
    /// legitimately failed its bar, with no error anywhere to notice.
    #[test]
    fn truncation_refuses_rather_than_decoding_to_a_shorter_extent() {
        let tmp = tempfile::tempdir().unwrap();
        let bytes = pack(0, &[vec![1u8; 50], vec![2u8; 50], vec![3u8; 50]]);

        for cut in [1usize, HEADER_LEN, HEADER_LEN + 8, bytes.len() - 1] {
            let path = write(tmp.path(), &format!("cut{cut}.tsmb"), &bytes[..cut]);
            assert!(
                MembershipPack::open(&path).is_err(),
                "a file cut to {cut} bytes must refuse"
            );
        }
        // And the whole file still opens, so the loop above is testing truncation rather than a
        // format that never opens at all.
        let path = write(tmp.path(), "whole.tsmb", &bytes);
        assert_eq!(MembershipPack::open(&path).unwrap().count(), 3);
    }

    #[test]
    fn a_foreign_or_future_file_refuses() {
        let tmp = tempfile::tempdir().unwrap();
        let mut bytes = pack(0, &[vec![7u8; 10]]);

        let mut wrong_magic = bytes.clone();
        wrong_magic[0] = b'X';
        let path = write(tmp.path(), "magic.tsmb", &wrong_magic);
        assert!(MembershipPack::open(&path).is_err());

        // A future version refuses rather than being read on this version's rules — the offsets
        // could mean something else entirely, and a plausible wrong answer is the failure mode.
        bytes[4] = 99;
        let path = write(tmp.path(), "version.tsmb", &bytes);
        assert!(MembershipPack::open(&path).is_err());
    }

    // ---- the containment partition ---------------------------------------------------------

    /// One small partition: two ordinals, three pairs, two expressions.
    fn containment_bytes(id_width: u8) -> Vec<u8> {
        // Expression 0: one clause `{1, 2}`. Expression 1: two clauses, `{1}` and `{3}`.
        let words = vec![1u32, 2, 1, 2, /* expr 1 */ 2, 1, 1, 1, 3];
        let expr_at = vec![0u32, 4, 9];
        pack_containment(id_width, &[0, 2, 3], &[0, 1, 0], &expr_at, &words)
    }

    /// **A mapped file and the same bytes in memory are read by one reader**, so a partition the
    /// fold wrote and one a publication built cannot be checked by different rules.
    #[test]
    fn a_partition_round_trips_through_the_map_and_the_buffer() {
        for id_width in [2u8, 4] {
            let bytes = containment_bytes(id_width);
            let tmp = tempfile::tempdir().unwrap();
            let path = write(tmp.path(), "partition.tscp", &bytes);
            let mapped = ContainmentPack::open(&path).unwrap();
            let owned = ContainmentPack::from_bytes(bytes.clone()).unwrap();
            for pack in [&mapped, &owned] {
                assert_eq!(pack.ordinals(), 2);
                assert_eq!(pack.pairs(), 3);
                assert_eq!(pack.expressions(), 2);
                assert_eq!(pack.id_width(), id_width);
                assert_eq!((pack.at(0), pack.at(1), pack.at(2)), (0, 2, 3));
                assert_eq!((pack.id(0), pack.id(1), pack.id(2)), (0, 1, 0));
                assert_eq!(pack.expression_at(1), 4);
                assert_eq!(pack.word(0), 1);
                assert_eq!(pack.word(4), 2);
            }
            assert_eq!(mapped.as_bytes(), owned.as_bytes());
        }
    }

    /// Every framing fault refuses. A partition read short answers *contained* for clauses it
    /// never tested, which is the permissive direction on the one test that must be conservative.
    #[test]
    fn a_torn_or_foreign_partition_refuses() {
        let tmp = tempfile::tempdir().unwrap();
        let good = containment_bytes(4);

        let mut wrong_magic = good.clone();
        wrong_magic[0] = b'X';
        assert!(ContainmentPack::from_bytes(wrong_magic).is_err());

        // A membership extent offered where a partition belongs: the distinct magic is what makes
        // that a refusal rather than a header that happens to parse.
        assert!(ContainmentPack::from_bytes(pack(0, &[vec![7u8; 8]])).is_err());

        let mut future = good.clone();
        future[4] = 99;
        assert!(ContainmentPack::from_bytes(future).is_err());

        let mut odd_width = good.clone();
        odd_width[6] = 3;
        assert!(ContainmentPack::from_bytes(odd_width).is_err());

        let mut truncated = good.clone();
        truncated.truncate(good.len() - 4);
        let path = write(tmp.path(), "short.tscp", &truncated);
        assert!(ContainmentPack::open(&path).is_err());

        // An identifier naming an expression the table does not hold — a truncation of the
        // identifier column read as a valid one, which is the failure the promotion rule exists
        // to prevent and this check catches from the other side.
        let mut wild = good.clone();
        let ids_at = CONTAINMENT_HEADER_LEN + 3 * 4;
        wild[ids_at..ids_at + 4].copy_from_slice(&9u32.to_le_bytes());
        assert!(ContainmentPack::from_bytes(wild).is_err());

        // A clause length reaching past its own expression.
        let mut overrun = good.clone();
        let words_at = overrun.len() - 9 * 4;
        overrun[words_at + 4..words_at + 8].copy_from_slice(&99u32.to_le_bytes());
        assert!(ContainmentPack::from_bytes(overrun).is_err());

        // Ordinal offsets that do not span the column.
        let mut short_span = good.clone();
        let at = CONTAINMENT_HEADER_LEN + 2 * 4;
        short_span[at..at + 4].copy_from_slice(&2u32.to_le_bytes());
        assert!(ContainmentPack::from_bytes(short_span).is_err());
    }

    #[test]
    fn offsets_that_go_backwards_refuse() {
        let tmp = tempfile::tempdir().unwrap();
        let mut bytes = pack(0, &[vec![1u8; 40], vec![2u8; 40]]);
        // Rewrite offsets[1] to point past offsets[2]. A reader that did not check would hand back
        // a slice with `from > to`, which panics, or — with the arithmetic reordered — one blob's
        // bytes under the other's ordinal.
        let at = HEADER_LEN + 8;
        bytes[at..at + 8].copy_from_slice(&999u64.to_le_bytes());
        let path = write(tmp.path(), "backwards.tsmb", &bytes);
        assert!(MembershipPack::open(&path).is_err());
    }

    // ---- the tile index's extent column -------------------------------------------------------

    fn tile_index_bytes() -> Vec<u8> {
        pack_tile_index(
            4_096,
            &[(0, 1_023), TILE_INDEX_HOLE, TILE_INDEX_EMPTY, (7, 4_095)],
        )
    }

    /// **A mapped file and the same bytes in memory are read by one reader**, and the two sentinels
    /// survive the round trip as different facts — which is the whole reason there are two of them.
    #[test]
    fn a_tile_index_round_trips_and_keeps_its_two_sentinels_apart() {
        let bytes = tile_index_bytes();
        let tmp = tempfile::tempdir().unwrap();
        let path = write(tmp.path(), "index.tsti", &bytes);
        let mapped = TileIndexPack::open(&path).unwrap();
        let owned = TileIndexPack::from_bytes(bytes.clone()).unwrap();
        for pack in [&mapped, &owned] {
            assert_eq!(pack.ordinals(), 4);
            assert_eq!(pack.row_count(), 4_096);
            assert_eq!(pack.span(0), (0, 1_023));
            assert_eq!(pack.span(1), TILE_INDEX_HOLE);
            assert_eq!(pack.span(2), TILE_INDEX_EMPTY);
            assert_ne!(pack.span(1), pack.span(2));
            assert_eq!(pack.span(3), (7, 4_095));
        }
        assert_eq!(mapped.as_bytes(), owned.as_bytes());
    }

    /// The header's row count is never below the highest row a span names, so the framing check is
    /// a check on the *file* rather than on the row space that produced it.
    #[test]
    fn the_row_count_covers_every_span_the_column_holds() {
        let pack = TileIndexPack::from_bytes(pack_tile_index(10, &[(0, 5_000)])).unwrap();
        assert_eq!(pack.row_count(), 5_001);
        // And an empty level keeps the row count it was handed.
        let empty = TileIndexPack::from_bytes(pack_tile_index(64, &[])).unwrap();
        assert_eq!((empty.ordinals(), empty.row_count()), (0, 64));
    }

    /// Every framing fault refuses. A column read short leaves every ordinal past the truncation
    /// out of the walk, so those artifacts stop being served with nothing reporting a fault.
    #[test]
    fn a_torn_or_foreign_tile_index_refuses() {
        let tmp = tempfile::tempdir().unwrap();
        let good = tile_index_bytes();

        let mut wrong_magic = good.clone();
        wrong_magic[0] = b'X';
        assert!(TileIndexPack::from_bytes(wrong_magic).is_err());

        // The two neighbouring formats offered where this one belongs: the distinct magic is what
        // makes each a refusal rather than a header that happens to parse.
        assert!(TileIndexPack::from_bytes(pack(0, &[vec![7u8; 8]])).is_err());
        assert!(TileIndexPack::from_bytes(containment_bytes(4)).is_err());
        assert!(ContainmentPack::from_bytes(good.clone()).is_err());

        let mut future = good.clone();
        future[4] = 99;
        assert!(TileIndexPack::from_bytes(future).is_err());

        let mut reserved = good.clone();
        reserved[6] = 1;
        assert!(TileIndexPack::from_bytes(reserved).is_err());

        let mut truncated = good.clone();
        truncated.truncate(good.len() - 8);
        let path = write(tmp.path(), "short.tsti", &truncated);
        assert!(TileIndexPack::open(&path).is_err());

        // A span that runs backwards without being either sentinel — a wrong answer in both
        // directions at once, since neither end of it addresses anything.
        let mut backwards = good.clone();
        let at = TILE_INDEX_HEADER_LEN;
        backwards[at..at + 4].copy_from_slice(&900u32.to_le_bytes());
        backwards[at + 4..at + 8].copy_from_slice(&8u32.to_le_bytes());
        assert!(TileIndexPack::from_bytes(backwards).is_err());

        // A span reaching past the row space the header declares: the node hierarchy folded over
        // it would have levels no walk visits, so the artifact would be silently unreachable.
        let mut past_the_end = good.clone();
        let at = TILE_INDEX_HEADER_LEN + 3 * 8 + 4;
        past_the_end[at..at + 4].copy_from_slice(&9_999u32.to_le_bytes());
        assert!(TileIndexPack::from_bytes(past_the_end).is_err());
    }

    // ---- the row-major columns ----------------------------------------------------------------

    /// **The width is chosen by the artifact count, one value tighter than the obvious rule**,
    /// because the all-ones value at each width is the hole.
    #[test]
    fn the_width_leaves_room_for_the_hole() {
        assert_eq!(row_column_width(0), 1);
        assert_eq!(row_column_width(254), 1);
        // 255 ordinals would need the value 254 *and* 255-as-hole, so it moves up.
        assert_eq!(row_column_width(255), 2);
        assert_eq!(row_column_width(65_534), 2);
        assert_eq!(row_column_width(65_535), 4);
        assert_eq!(row_column_width(10_000_000), 4);
    }

    /// **A mapped file and the same bytes in memory are read by one reader**, and the hole survives
    /// the round trip at every width — a row belonging to no artifact is most rows of most layers,
    /// and a reader that lost the distinction would count them against ordinal 0.
    #[test]
    fn a_label_column_round_trips_at_every_width() {
        let tmp = tempfile::tempdir().unwrap();
        for (ordinals, width) in [(3u32, 1u8), (300, 2), (70_000, 4)] {
            let labels = vec![0, ROW_COLUMN_HOLE, ordinals - 1, 1, ROW_COLUMN_HOLE];
            let bytes = pack_label_column(ordinals, &labels);
            let path = write(tmp.path(), &format!("labels{width}.tslb"), &bytes);
            let mapped = LabelColumnPack::open(&path).unwrap();
            let owned = LabelColumnPack::from_bytes(bytes.clone()).unwrap();
            for pack in [&mapped, &owned] {
                assert_eq!(pack.width(), width);
                assert_eq!(pack.rows(), 5);
                assert_eq!(pack.ordinals(), ordinals);
                for (row, expected) in labels.iter().enumerate() {
                    assert_eq!(pack.label(row), *expected, "row {row} at width {width}");
                }
                // A row past the column is a hole, not a panic and not ordinal 0.
                assert_eq!(pack.label(5), ROW_COLUMN_HOLE);
            }
            assert_eq!(mapped.as_bytes(), owned.as_bytes());
        }
    }

    /// The list form, with an empty list beside a multi-entry one — the two states the label form
    /// cannot express, and the whole reason it exists.
    #[test]
    fn a_list_column_round_trips_with_empty_and_shared_rows() {
        let tmp = tempfile::tempdir().unwrap();
        // row 0 -> {0, 2}, row 1 -> {}, row 2 -> {1}, row 3 -> {}
        let at = vec![0u32, 2, 2, 3, 3];
        let values = vec![0u32, 2, 1];
        let bytes = pack_list_column(3, &at, &values);
        let path = write(tmp.path(), "lists.tsll", &bytes);
        let mapped = ListColumnPack::open(&path).unwrap();
        let owned = ListColumnPack::from_bytes(bytes.clone()).unwrap();
        for pack in [&mapped, &owned] {
            assert_eq!(pack.rows(), 4);
            assert_eq!(pack.ordinals(), 3);
            assert_eq!(pack.entries(), 3);
            assert_eq!(pack.list(0).collect::<Vec<_>>(), vec![0, 2]);
            assert!(pack.list(1).next().is_none());
            assert_eq!(pack.list(2).collect::<Vec<_>>(), vec![1]);
            assert!(pack.list(3).next().is_none());
            assert!(
                pack.list(9).next().is_none(),
                "a row past the column is empty"
            );
        }
        assert_eq!(mapped.as_bytes(), owned.as_bytes());
    }

    /// Every framing fault refuses. A column read short leaves rows belonging to nobody, so the
    /// artifacts holding them stop being candidates with nothing reporting a fault; a label read
    /// wide counts one artifact's rows under another's identity.
    #[test]
    fn a_torn_or_foreign_row_column_refuses() {
        let tmp = tempfile::tempdir().unwrap();
        let good = pack_label_column(300, &[0, ROW_COLUMN_HOLE, 299, 1]);

        let mut wrong_magic = good.clone();
        wrong_magic[0] = b'X';
        assert!(LabelColumnPack::from_bytes(wrong_magic).is_err());

        // The neighbouring formats offered where this one belongs, in both directions: the distinct
        // magics are what make each a refusal rather than a header that happens to parse.
        assert!(LabelColumnPack::from_bytes(tile_index_bytes()).is_err());
        assert!(LabelColumnPack::from_bytes(containment_bytes(4)).is_err());
        assert!(LabelColumnPack::from_bytes(pack(0, &[vec![7u8; 8]])).is_err());
        assert!(ListColumnPack::from_bytes(good.clone()).is_err());
        assert!(LabelColumnPack::from_bytes(pack_list_column(2, &[0, 1], &[0])).is_err());
        assert!(TileIndexPack::from_bytes(good.clone()).is_err());
        assert!(ContainmentPack::from_bytes(good.clone()).is_err());

        let mut future = good.clone();
        future[4] = 99;
        assert!(LabelColumnPack::from_bytes(future).is_err());

        let mut odd_width = good.clone();
        odd_width[6] = 3;
        assert!(LabelColumnPack::from_bytes(odd_width).is_err());

        let mut reserved = good.clone();
        reserved[7] = 1;
        assert!(LabelColumnPack::from_bytes(reserved).is_err());

        let mut truncated = good.clone();
        truncated.truncate(good.len() - 2);
        let path = write(tmp.path(), "short.tslb", &truncated);
        assert!(LabelColumnPack::open(&path).is_err());

        // A label naming an ordinal the level does not hold — what a truncated identifier column
        // read as a valid one looks like from the other side.
        let mut wild = good.clone();
        let at = LABEL_HEADER_LEN;
        wild[at..at + 2].copy_from_slice(&999u16.to_le_bytes());
        assert!(LabelColumnPack::from_bytes(wild).is_err());

        // A width that cannot address the level it claims: 255 ordinals at width 1 leaves no value
        // for the hole, so a hole would read as ordinal 255.
        let mut narrow = good.clone();
        narrow[6] = 1;
        assert!(LabelColumnPack::from_bytes(narrow).is_err());

        // And the list form's own faults.
        let list = pack_list_column(3, &[0, 2, 2, 3, 3], &[0, 2, 1]);
        let mut backwards = list.clone();
        let at = LIST_HEADER_LEN + 4;
        backwards[at..at + 4].copy_from_slice(&3u32.to_le_bytes());
        assert!(ListColumnPack::from_bytes(backwards).is_err());

        let mut wild_entry = list.clone();
        let at = LIST_HEADER_LEN + 5 * 4;
        wild_entry[at] = 9;
        assert!(ListColumnPack::from_bytes(wild_entry).is_err());

        let mut short_list = list.clone();
        short_list.truncate(list.len() - 1);
        assert!(ListColumnPack::from_bytes(short_list).is_err());
    }

    /// **Each of the four width rules refuses on its own**, with the length the header describes
    /// left consistent so the generic length check cannot stand in for any of them.
    ///
    /// The corruptions in `a_torn_or_foreign_row_column_refuses` above cannot do this: both of its
    /// width faults change byte 6 of a column packed at width 2, which changes the length the
    /// header describes, so the length check refuses either of them whether the width rules are
    /// there or not — and the list column had no width fault aimed at it at all. Every framing
    /// refusal on these paths is the same `MalformedBundle`, so the assertions below read the
    /// `detail`; an error-kind check would be no stronger than `is_err()`.
    ///
    /// Mutations this kills: deleting either width rule from either column reader. Removing the
    /// width-set rule lets `label()`'s and the entry reader's `_ =>` arms read four bytes at a
    /// three-byte stride; removing the ordinals-vs-width rule admits a column whose hole value is
    /// also a legal ordinal, so rows carrying that ordinal read as holes and the artifacts holding
    /// them silently stop being candidates.
    #[test]
    fn each_row_column_width_rule_refuses_without_help_from_the_length_check() {
        fn detail(err: crate::StoreError) -> String {
            match err {
                crate::StoreError::MalformedBundle { detail } => detail,
                other => panic!("expected a malformed-bundle refusal, got {other}"),
            }
        }

        // Twelve labels at width 1 occupy 12 bytes, which is also four rows at width 3 — so the
        // header describes exactly the bytes present and only the width-set rule can refuse.
        let wide = pack_label_column(10, &[0, 1, 2, 3, 4, 5, 6, 7, 8, 9, 0, 1]);
        assert_eq!(wide.len(), LABEL_HEADER_LEN + 12, "packed at width 1");
        let mut odd_width = wide.clone();
        odd_width[6] = 3;
        odd_width[8..12].copy_from_slice(&4u32.to_le_bytes());
        assert_eq!(
            odd_width.len(),
            LABEL_HEADER_LEN + 4 * 3,
            "the header must describe exactly the bytes present, or the length check refuses \
             this instead and the width rule is untested"
        );
        let d = detail(LabelColumnPack::from_bytes(odd_width).unwrap_err());
        assert!(
            d.contains("expected 1, 2 or 4"),
            "a label width outside the set must be what refuses: {d}"
        );

        // The same column, its width left alone, claiming a level whose ordinal count reaches the
        // hole value. The bytes are untouched, so the length check has nothing to say.
        let good = pack_label_column(10, &[0, 1, 2, 9]);
        let mut narrow = good.clone();
        narrow[12..16].copy_from_slice(&255u32.to_le_bytes());
        assert_eq!(narrow.len(), good.len(), "only the header field moved");
        let d = detail(LabelColumnPack::from_bytes(narrow).unwrap_err());
        assert!(
            d.contains("cannot be addressed at width 1"),
            "255 ordinals at width 1 leaves no value for the hole: {d}"
        );

        // The list column carries the identical pair. Four entries at width 1 over four rows is
        // 44 bytes; two rows at width 3 is 44 bytes as well, and the offsets still span the value
        // column at the shortened row count, so nothing downstream of the width rule refuses.
        let list = pack_list_column(3, &[0, 2, 4, 4, 4], &[0, 1, 2, 0]);
        assert_eq!(list.len(), LIST_HEADER_LEN + 5 * 4 + 4, "packed at width 1");
        let mut odd_width = list.clone();
        odd_width[6] = 3;
        odd_width[8..12].copy_from_slice(&2u32.to_le_bytes());
        assert_eq!(
            odd_width.len(),
            LIST_HEADER_LEN + 3 * 4 + 4 * 3,
            "the header must describe exactly the bytes present, or the length check refuses \
             this instead and the width rule is untested"
        );
        let d = detail(ListColumnPack::from_bytes(odd_width).unwrap_err());
        assert!(
            d.contains("expected 1, 2 or 4"),
            "an entry width outside the set must be what refuses: {d}"
        );

        let mut narrow = list.clone();
        narrow[12..16].copy_from_slice(&255u32.to_le_bytes());
        assert_eq!(narrow.len(), list.len(), "only the header field moved");
        let d = detail(ListColumnPack::from_bytes(narrow).unwrap_err());
        assert!(
            d.contains("cannot be addressed at width 1"),
            "255 ordinals at width 1 leaves no value for the hole: {d}"
        );
    }
}
