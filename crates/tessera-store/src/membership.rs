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
                format!("{} bytes is shorter than the {HEADER_LEN}-byte header", map.len()),
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
