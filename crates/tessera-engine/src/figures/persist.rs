//! A fragment's counts and a level's denied-row labels on disk, in the engine's cache directory,
//! so a restart reads them back rather than walking again.
//!
//! One directory per bundle identity, and a file per entry named by a digest of what it is a
//! function of and the level version it describes. A file is a magic, the payload's length and
//! its SHA-256, then the payload; a file whose length or digest does not match is not read, so a
//! torn or altered entry is a miss. Writes go through [`tessera_authz::write_private_atomically`],
//! as the fragments beside them do.
//!
//! The directory is held under a byte bound: after each write the files least recently written or
//! read are removed until what is left fits.

use std::path::{Path, PathBuf};

use rustc_hash::FxHashMap;
use sha2::{Digest, Sha256};

use crate::engine::hex_encode as hex;
use crate::row_column::RESERVE;

use super::counts::{CountsAt, Dense, Grown, Reserves};
use super::denied::DeniedLabels;
use super::field::{FieldTally, Number};
use super::Geometry;

const COUNTS_MAGIC: &[u8; 8] = b"TSFCNT02";
const DENIED_MAGIC: &[u8; 8] = b"TSFDNY02";
const FIELD_MAGIC: &[u8; 8] = b"TSFFLD01";
const HEADER: usize = 8 + 8 + 32;

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

/// What one field's tally over a fragment's base rows is a function of, as a file stem.
pub(crate) fn field_stem(terms: &[u8; 32], view: &str, column: &str, kind: u8) -> String {
    let mut hasher = Sha256::new();
    hasher.update(b"field");
    hasher.update(terms);
    for text in [view, column] {
        hasher.update((text.len() as u64).to_le_bytes());
        hasher.update(text.as_bytes());
    }
    hasher.update([kind]);
    hex(&hasher.finalize())
}

fn path_of(dir: &Path, stem: &str, at: u64, extension: &str) -> PathBuf {
    dir.join(format!("{stem}-{at}.{extension}"))
}

/// Remove the files under `root` least recently written or read until they hold at most `bound`
/// bytes.
pub(crate) fn hold_under(root: &Path, bound: u64) {
    fn walk(dir: &Path, out: &mut Vec<(std::time::SystemTime, u64, PathBuf)>) {
        let Ok(entries) = std::fs::read_dir(dir) else {
            return;
        };
        for entry in entries.flatten() {
            let Ok(meta) = entry.metadata() else { continue };
            if meta.is_dir() {
                walk(&entry.path(), out);
            } else {
                let at = meta.modified().unwrap_or(std::time::UNIX_EPOCH);
                out.push((at, meta.len(), entry.path()));
            }
        }
    }
    let mut files = Vec::new();
    walk(root, &mut files);
    let mut held: u64 = files.iter().map(|(_, len, _)| len).sum();
    files.sort();
    for (_, len, path) in files {
        if held <= bound {
            break;
        }
        if std::fs::remove_file(&path).is_ok() {
            held -= len;
        }
    }
}

/// The bytes the files under `root` hold.
pub(crate) fn bytes_under(root: &Path) -> u64 {
    let Ok(entries) = std::fs::read_dir(root) else {
        return 0;
    };
    entries
        .flatten()
        .map(|entry| match entry.metadata() {
            Ok(meta) if meta.is_dir() => bytes_under(&entry.path()),
            Ok(meta) => meta.len(),
            Err(_) => 0,
        })
        .sum()
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

/// The payload of the entry `stem` at `at`, where a whole and unaltered one is on disk. A read marks
/// the file used, so the byte bound removes it after the files nobody has read.
fn read(dir: &Path, stem: &str, at: u64, extension: &str, magic: &[u8; 8]) -> Option<Vec<u8>> {
    let path = path_of(dir, stem, at, extension);
    let bytes = std::fs::read(&path).ok()?;
    if let Ok(file) = std::fs::File::options().append(true).open(&path) {
        let _ = file.set_modified(std::time::SystemTime::now());
    }
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

/// Little-endian reads off the front of a payload. `tessera-store`'s readers are private to the
/// formats they frame, so this is the figures' own.
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

fn put_u32s(payload: &mut Vec<u8>, values: &[u32]) {
    for v in values {
        payload.extend_from_slice(&v.to_le_bytes());
    }
}

fn put_u64s(payload: &mut Vec<u8>, values: &[u64]) {
    for v in values {
        payload.extend_from_slice(&v.to_le_bytes());
    }
}

/// Write a fragment's counts at their version: the walk, its reserves, and what the steps since it
/// added.
pub(crate) fn write_counts(dir: &Path, stem: &str, counts: &CountsAt, reserves: Option<&Reserves>) {
    let dense = &counts.dense;
    let n = dense.counts.len();
    let geometry = !dense.placed.is_empty();
    let mut payload = Vec::with_capacity(n * if geometry { 40 } else { 4 } + 64);
    payload.extend_from_slice(&dense.filled_at.to_le_bytes());
    payload.extend_from_slice(&(n as u64).to_le_bytes());
    payload.push(u8::from(geometry) | (u8::from(reserves.is_some()) << 1));
    put_u32s(&mut payload, &dense.counts);
    if geometry {
        put_u32s(&mut payload, &dense.placed);
        put_u64s(&mut payload, dense.sums.as_flattened());
        put_u32s(&mut payload, dense.boxes.as_flattened());
    }
    if let Some(reserves) = reserves {
        payload.extend_from_slice(&(reserves.index.len() as u64).to_le_bytes());
        put_u32s(&mut payload, &reserves.index);
        payload.extend_from_slice(&(reserves.rows.len() as u64).to_le_bytes());
        for rows in &reserves.rows {
            put_u32s(&mut payload, rows.as_flattened());
        }
    }
    let mut grown: Vec<(&u32, &Grown)> = counts.grown.iter().collect();
    grown.sort_unstable_by_key(|(ordinal, _)| **ordinal);
    payload.extend_from_slice(&(grown.len() as u64).to_le_bytes());
    for (ordinal, g) in grown {
        put_u32s(&mut payload, &[*ordinal, g.count, g.placed]);
        put_u64s(&mut payload, &g.sums);
        payload.extend_from_slice(&(g.points.len() as u64).to_le_bytes());
        for &(row, x, y) in &g.points {
            put_u32s(&mut payload, &[row, x, y]);
        }
    }
    write(dir, stem, counts.at, "counts", COUNTS_MAGIC, &payload);
}

/// A fragment's counts at `at`, and their reserves, where they are on disk with the geometry
/// `geometry` asks for.
pub(crate) fn read_counts(
    dir: &Path,
    stem: &str,
    at: u64,
    geometry: Geometry,
) -> Option<(CountsAt, Option<Reserves>)> {
    let payload = read(dir, stem, at, "counts", COUNTS_MAGIC)?;
    let mut r = Reader(&payload);
    let filled_at = r.u64()?;
    let n = usize::try_from(r.u64()?).ok()?;
    let flags = r.u8()?;
    let (placed, reserved) = (flags & 1 != 0, flags & 2 != 0);
    if placed != (geometry != Geometry::None) || (reserved && geometry != Geometry::Box) {
        return None;
    }
    let mut dense = Dense {
        filled_at,
        counts: r.u32s(n)?,
        ..Dense::default()
    };
    if placed {
        dense.placed = r.u32s(n)?;
        dense.sums = r.u64s(2 * n)?.as_chunks::<2>().0.to_vec();
        dense.boxes = r.u32s(4 * n)?.as_chunks::<4>().0.to_vec();
    }
    let reserves = match reserved {
        false => None,
        true => {
            let len = usize::try_from(r.u64()?).ok()?;
            let index = r.u32s(len)?;
            let held = usize::try_from(r.u64()?).ok()?;
            let flat = r.u32s(held.checked_mul(4 * RESERVE)?)?;
            let sides = flat.as_chunks::<RESERVE>().0;
            Some(Reserves {
                index,
                rows: sides.as_chunks::<4>().0.to_vec(),
            })
        }
    };
    let mut grown = FxHashMap::default();
    for _ in 0..r.u64()? {
        let head = r.u32s(3)?;
        let sums = r.u64s(2)?;
        let points = usize::try_from(r.u64()?).ok()?;
        let flat = r.u32s(points.checked_mul(3)?)?;
        grown.insert(
            head[0],
            Grown {
                count: head[1],
                placed: head[2],
                sums: [sums[0], sums[1]],
                points: flat
                    .as_chunks::<3>()
                    .0
                    .iter()
                    .map(|p| (p[0], p[1], p[2]))
                    .collect(),
            },
        );
    }
    if !r.0.is_empty() {
        return None;
    }
    let counts = CountsAt {
        at,
        dense: std::sync::Arc::new(dense),
        grown: std::sync::Arc::new(grown),
    };
    Some((counts, reserves))
}

/// Write a level's denied-row labels.
pub(crate) fn write_denied(dir: &Path, stem: &str, labels: &DeniedLabels) {
    let n = labels.rows.len();
    let mut payload = Vec::with_capacity(n * 24 + 32);
    payload.extend_from_slice(&(n as u64).to_le_bytes());
    put_u32s(&mut payload, &labels.rows);
    put_u32s(&mut payload, &labels.offsets);
    payload.extend_from_slice(&(labels.ordinals.len() as u64).to_le_bytes());
    put_u32s(&mut payload, &labels.ordinals);
    for position in &labels.positions {
        let (x, y) = position.unwrap_or((0, 0));
        payload.push(u8::from(position.is_some()));
        put_u32s(&mut payload, &[x, y]);
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
    let n = usize::try_from(r.u64()?).ok()?;
    let rows = r.u32s(n)?;
    let offsets = r.u32s(n.checked_add(1)?)?;
    let total = usize::try_from(r.u64()?).ok()?;
    let ordinals = r.u32s(total)?;
    let mut positions = Vec::with_capacity(n);
    for _ in 0..n {
        let placed = r.u8()? != 0;
        let (x, y) = (r.u32()?, r.u32()?);
        positions.push(placed.then_some((x, y)));
    }
    let framed = r.0.is_empty()
        && rows.windows(2).all(|w| w[0] < w[1])
        && offsets.windows(2).all(|w| w[0] <= w[1])
        && offsets.first() == Some(&0)
        && offsets.last().map(|&o| o as usize) == Some(total);
    framed.then_some(DeniedLabels {
        identity,
        column,
        at,
        rows,
        offsets,
        ordinals,
        positions,
    })
}

fn put_number(payload: &mut Vec<u8>, value: Number) {
    let bits = match value {
        Number::Int(i) => i as u128,
        Number::Float(f) => u128::from(f.to_bits()),
    };
    payload.extend_from_slice(&bits.to_le_bytes());
}

/// Write a field's tally over a fragment's base rows. Nothing changes it between folds, so it has
/// one version.
pub(crate) fn write_field(dir: &Path, stem: &str, tally: &FieldTally) {
    let float = matches!(tally.sum, Number::Float(_));
    let mut payload = Vec::with_capacity(64 + 20 * (tally.low.len() + tally.high.len()));
    payload.push(u8::from(float));
    payload.extend_from_slice(&tally.none.to_le_bytes());
    payload.extend_from_slice(&tally.count.to_le_bytes());
    put_number(&mut payload, tally.sum);
    for side in [&tally.low, &tally.high] {
        payload.extend_from_slice(&(side.len() as u64).to_le_bytes());
        for &(value, row) in side.iter() {
            put_number(&mut payload, value);
            payload.extend_from_slice(&row.to_le_bytes());
        }
    }
    write(dir, stem, 0, "field", FIELD_MAGIC, &payload);
}

/// A field's tally over a fragment's base rows, where it is on disk with values of the kind
/// `float` says.
pub(crate) fn read_field(dir: &Path, stem: &str, float: bool) -> Option<FieldTally> {
    let payload = read(dir, stem, 0, "field", FIELD_MAGIC)?;
    let mut r = Reader(&payload);
    if (r.u8()? != 0) != float {
        return None;
    }
    let number = |r: &mut Reader<'_>| -> Option<Number> {
        let bits = u128::from_le_bytes(r.take(16)?.try_into().ok()?);
        Some(match float {
            true => Number::Float(f64::from_bits(bits as u64)),
            false => Number::Int(bits as i128),
        })
    };
    let none = r.u64()?;
    let count = r.u64()?;
    let sum = number(&mut r)?;
    let mut sides = [Vec::new(), Vec::new()];
    for side in &mut sides {
        let n = usize::try_from(r.u64()?).ok()?;
        for _ in 0..n.min(payload.len()) {
            let value = number(&mut r)?;
            side.push((value, r.u32()?));
        }
    }
    let [low, high] = sides;
    r.0.is_empty().then_some(FieldTally {
        none,
        count,
        sum,
        low,
        high,
    })
}
