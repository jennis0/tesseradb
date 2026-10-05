//! A fragment's counts and a level's denied-row labels on disk, in the engine's cache directory,
//! so a restart reads them back rather than walking again.
//!
//! One directory per bundle identity, and a file per entry named by a digest of what it is a
//! function of and the level version it describes. A file is a magic, the payload's length and
//! its SHA-256, then the payload; a file whose length or digest does not match is not read, so a
//! torn or altered entry is a miss. Writes go through [`tessera_authz::write_private_atomically`],
//! as the fragments beside them do.

use std::path::{Path, PathBuf};

use croaring::Bitmap;
use rustc_hash::FxHashMap;
use sha2::{Digest, Sha256};

use crate::row_column::{Reserve, RESERVE};

use super::counts::Dense;
use super::denied::{DeniedLabels, Labelled};
use super::Geometry;

const COUNTS_MAGIC: &[u8; 8] = b"TSFCNT01";
const DENIED_MAGIC: &[u8; 8] = b"TSFDNY01";
const HEADER: usize = 8 + 8 + 32;

fn hex(bytes: &[u8]) -> String {
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}

/// The directory a bundle identity's entries live in.
pub(crate) fn identity_dir(root: &Path, identity: &[u8; 32]) -> PathBuf {
    root.join(hex(identity))
}

/// Remove every identity's directory but `identity`'s: the entries of a superseded bundle can never
/// be read again.
pub(crate) fn sweep_other_identities(root: &Path, identity: &[u8; 32]) {
    let keep = hex(identity);
    let Ok(entries) = std::fs::read_dir(root) else {
        return;
    };
    for entry in entries.flatten() {
        if entry.file_name().to_string_lossy() != keep {
            let _ = std::fs::remove_dir_all(entry.path());
        }
    }
}

/// What one fragment's counts are a function of, as a file stem.
pub(crate) fn counts_stem(
    terms: &[u8; 32],
    view: &str,
    layer: &str,
    level: u32,
    geometry: Geometry,
) -> String {
    let mut hasher = Sha256::new();
    hasher.update(b"counts");
    hasher.update(terms);
    for text in [view, layer] {
        hasher.update((text.len() as u64).to_le_bytes());
        hasher.update(text.as_bytes());
    }
    hasher.update(level.to_le_bytes());
    hasher.update([geometry as u8]);
    hex(&hasher.finalize())
}

/// What one level's denied-row labels in one view are a function of, as a file stem.
pub(crate) fn denied_stem(view: &str, layer: &str, level: u32) -> String {
    let mut hasher = Sha256::new();
    hasher.update(b"denied");
    for text in [view, layer] {
        hasher.update((text.len() as u64).to_le_bytes());
        hasher.update(text.as_bytes());
    }
    hasher.update(level.to_le_bytes());
    hex(&hasher.finalize())
}

fn path_of(dir: &Path, stem: &str, at: u64, extension: &str) -> PathBuf {
    dir.join(format!("{stem}-{at}.{extension}"))
}

/// Write `payload` as the entry `stem` at `at`, and remove the entry's other versions.
fn write(dir: &Path, stem: &str, at: u64, extension: &str, magic: &[u8; 8], payload: &[u8]) {
    let mut bytes = Vec::with_capacity(HEADER + payload.len());
    bytes.extend_from_slice(magic);
    bytes.extend_from_slice(&(payload.len() as u64).to_le_bytes());
    bytes.extend_from_slice(&Sha256::digest(payload));
    bytes.extend_from_slice(payload);
    let path = path_of(dir, stem, at, extension);
    if let Err(error) = tessera_authz::write_private_atomically(&path, &bytes) {
        tracing::warn!(
            path = %path.display(),
            %error,
            "a level's figures could not be written to the cache directory; a restart walks them again"
        );
        return;
    }
    let Ok(entries) = std::fs::read_dir(dir) else {
        return;
    };
    let prefix = format!("{stem}-");
    let suffix = format!(".{extension}");
    let current = format!("{stem}-{at}.{extension}");
    for entry in entries.flatten() {
        let name = entry.file_name().to_string_lossy().into_owned();
        if name.starts_with(&prefix) && name.ends_with(&suffix) && name != current {
            let _ = std::fs::remove_file(entry.path());
        }
    }
}

/// The payload of the entry `stem` at `at`, where a whole and unaltered one is on disk.
fn read(dir: &Path, stem: &str, at: u64, extension: &str, magic: &[u8; 8]) -> Option<Vec<u8>> {
    let bytes = std::fs::read(path_of(dir, stem, at, extension)).ok()?;
    if bytes.len() < HEADER || &bytes[..8] != magic {
        return None;
    }
    let length = u64::from_le_bytes(bytes[8..16].try_into().ok()?) as usize;
    let payload = &bytes[HEADER..];
    if payload.len() != length || Sha256::digest(payload).as_slice() != &bytes[16..48] {
        return None;
    }
    Some(payload.to_vec())
}

/// Little-endian reads off the front of a payload.
struct Reader<'a>(&'a [u8]);

impl Reader<'_> {
    fn take(&mut self, n: usize) -> Option<&[u8]> {
        if self.0.len() < n {
            return None;
        }
        let (head, rest) = self.0.split_at(n);
        self.0 = rest;
        Some(head)
    }
    fn u8(&mut self) -> Option<u8> {
        Some(self.take(1)?[0])
    }
    fn u32(&mut self) -> Option<u32> {
        Some(u32::from_le_bytes(self.take(4)?.try_into().ok()?))
    }
    fn u64(&mut self) -> Option<u64> {
        Some(u64::from_le_bytes(self.take(8)?.try_into().ok()?))
    }
    fn u32s(&mut self, n: usize) -> Option<Vec<u32>> {
        let bytes = self.take(n.checked_mul(4)?)?;
        Some(
            bytes
                .as_chunks::<4>()
                .0
                .iter()
                .map(|c| u32::from_le_bytes(*c))
                .collect(),
        )
    }
    fn u64s(&mut self, n: usize) -> Option<Vec<u64>> {
        let bytes = self.take(n.checked_mul(8)?)?;
        Some(
            bytes
                .as_chunks::<8>()
                .0
                .iter()
                .map(|c| u64::from_le_bytes(*c))
                .collect(),
        )
    }
}

/// Write a fragment's counts at `at`.
pub(crate) fn write_counts(dir: &Path, stem: &str, at: u64, dense: &Dense) {
    let n = dense.counts.len();
    let geometry = !dense.placed.is_empty();
    let reserves = !dense.reserves.is_empty();
    let mut payload = Vec::with_capacity(n * (4 + if geometry { 36 } else { 0 }) + 16);
    payload.extend_from_slice(&(n as u64).to_le_bytes());
    payload.push(u8::from(geometry) | (u8::from(reserves) << 1));
    let u32s = |values: &[u32], payload: &mut Vec<u8>| {
        for v in values {
            payload.extend_from_slice(&v.to_le_bytes());
        }
    };
    u32s(&dense.counts, &mut payload);
    if geometry {
        u32s(&dense.placed, &mut payload);
        for s in &dense.sums {
            for v in s {
                payload.extend_from_slice(&v.to_le_bytes());
            }
        }
        for b in &dense.boxes {
            u32s(b, &mut payload);
        }
    }
    if reserves {
        for reserve in &dense.reserves {
            for side in reserve {
                for key in side {
                    payload.extend_from_slice(&key.to_le_bytes());
                }
            }
        }
    }
    write(dir, stem, at, "counts", COUNTS_MAGIC, &payload);
}

/// A fragment's counts at `at`, where they are on disk with the geometry `geometry` asks for.
pub(crate) fn read_counts(dir: &Path, stem: &str, at: u64, geometry: Geometry) -> Option<Dense> {
    let payload = read(dir, stem, at, "counts", COUNTS_MAGIC)?;
    let mut r = Reader(&payload);
    let n = usize::try_from(r.u64()?).ok()?;
    let flags = r.u8()?;
    let (placed, reserves) = (flags & 1 != 0, flags & 2 != 0);
    if placed != (geometry != Geometry::None) || reserves != (geometry == Geometry::Box) {
        return None;
    }
    let mut dense = Dense {
        counts: r.u32s(n)?,
        ..Dense::default()
    };
    if placed {
        dense.placed = r.u32s(n)?;
        dense.sums = r
            .u64s(2 * n)?
            .as_chunks::<2>()
            .0
            .to_vec();
        dense.boxes = r
            .u32s(4 * n)?
            .as_chunks::<4>()
            .0
            .to_vec();
    }
    if reserves {
        let keys = r.u64s(n * 4 * RESERVE)?;
        let sides = keys.as_chunks::<RESERVE>().0;
        let reserves: &[Reserve] = sides.as_chunks::<4>().0;
        dense.reserves = reserves.to_vec();
    }
    r.0.is_empty().then_some(dense)
}

/// Write a level's denied-row labels.
pub(crate) fn write_denied(dir: &Path, stem: &str, labels: &DeniedLabels) {
    let mut payload = Vec::with_capacity(labels.labels.len() * 24 + 8);
    payload.extend_from_slice(&(labels.labels.len() as u64).to_le_bytes());
    let mut rows: Vec<(&u32, &Labelled)> = labels.labels.iter().collect();
    rows.sort_unstable_by_key(|(row, _)| **row);
    for (row, held) in rows {
        payload.extend_from_slice(&row.to_le_bytes());
        let (x, y) = held.position.unwrap_or((0, 0));
        payload.push(u8::from(held.position.is_some()));
        payload.extend_from_slice(&x.to_le_bytes());
        payload.extend_from_slice(&y.to_le_bytes());
        payload.extend_from_slice(&(held.ordinals.len() as u32).to_le_bytes());
        for ordinal in &held.ordinals {
            payload.extend_from_slice(&ordinal.to_le_bytes());
        }
    }
    write(dir, stem, labels.at, "denied", DENIED_MAGIC, &payload);
}

/// A level's denied-row labels at `at`, where they are on disk, filed under `identity` and
/// `column`, which name what the caller reads them for.
pub(crate) fn read_denied(
    dir: &Path,
    stem: &str,
    at: u64,
    identity: [u8; 32],
    column: u64,
) -> Option<DeniedLabels> {
    let payload = read(dir, stem, at, "denied", DENIED_MAGIC)?;
    let mut r = Reader(&payload);
    let n = r.u64()?;
    let mut labels = FxHashMap::default();
    let mut rows = Vec::new();
    for _ in 0..n {
        let row = r.u32()?;
        let placed = r.u8()? != 0;
        let (x, y) = (r.u32()?, r.u32()?);
        let count = r.u32()? as usize;
        let ordinals = r.u32s(count)?;
        rows.push(row);
        labels.insert(
            row,
            Labelled {
                ordinals,
                position: placed.then_some((x, y)),
            },
        );
    }
    if !r.0.is_empty() {
        return None;
    }
    Some(DeniedLabels {
        identity,
        column,
        at,
        rows: Bitmap::of(&rows),
        labels,
    })
}
