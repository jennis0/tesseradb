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
//! # This module knows nothing about bitmaps
//!
//! A membership is an opaque byte blob here, and deliberately: `tessera-lifecycle` owns the Roaring
//! form and does not depend on this crate, so a layout that understood the payload would put the
//! bitmap library on both sides of a boundary that currently has it on one. What this owns is
//! **addressing** — which bytes belong to which ordinal — and nothing else.
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
use std::path::Path;

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
}
