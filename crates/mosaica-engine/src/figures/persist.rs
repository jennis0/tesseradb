//! A level's sums, its sides' walks and its denied-row labels on disk, in the engine's cache
//! directory, so a restart reads them back rather than walking again.
//!
//! A level's sum for one grant is one file in [`SUMS_DIR`], which names the sides it is over.
//! Each side's walk, which a sum is taken again from when another side moves, and the
//! denied rows' labels are in one directory per shard identity. Every file is named by a digest of
//! what it is a function of and the version it describes. A file is a magic, the payload's length
//! and its SHA-256, then the payload; a file whose length or digest does not match is not read, so
//! a torn or altered entry is a miss. Writes go through
//! [`mosaica_authz::write_private_atomically`], as the fragments beside them do.
//!
//! The directory is held under a byte bound: after each write the files least recently written or
//! read are removed until what is left fits.

use std::path::{Path, PathBuf};

use mosaica_types::LayerSpace;
use rustc_hash::FxHashMap;
use sha2::{Digest, Sha256};

use crate::engine::hex_encode as hex;
use crate::row_column::RESERVE;

use super::counts::{CountsAt, Dense, Grown, Part, Reserves, SideAt, SideGrown, Wide};
use super::denied::DeniedLabels;
use super::Geometry;

const SUM_MAGIC: &[u8; 8] = b"MSFSUM05";
const PART_MAGIC: &[u8; 8] = b"MSFPRT04";
const DENIED_MAGIC: &[u8; 8] = b"MSFDNY03";

/// The subdirectory of the figures' directory the sums are written to.
pub(crate) const SUMS_DIR: &str = "sums";

/// The directory the sums are written to.
pub(crate) fn sums_dir(root: &Path) -> PathBuf {
    root.join(SUMS_DIR)
}
const HEADER: usize = 8 + 8 + 32;

/// The directory a shard identity's entries live in.
pub(crate) fn identity_dir(root: &Path, identity: &[u8; 32]) -> PathBuf {
    root.join(hex(identity))
}

/// Remove every identity's directory but those of `keep`: the entries of a superseded prefix can
/// never be read again. The sums are kept: one naming an identity removed here is walked again.
pub(crate) fn sweep_other_identities(root: &Path, keep: &[[u8; 32]]) {
    let mut keep: Vec<String> = keep.iter().map(|identity| hex(identity)).collect();
    keep.push(SUMS_DIR.to_string());
    let Ok(entries) = std::fs::read_dir(root) else {
        return;
    };
    for entry in entries.flatten() {
        if !keep.contains(&entry.file_name().to_string_lossy().into_owned()) {
            let _ = std::fs::remove_dir_all(entry.path());
        }
    }
}

/// What a level's figures for one grant are a function of beside their sides, as a file stem, the
/// layer named by its space as [`super::cache::SumKey`] names it.
pub(crate) fn counts_stem(
    terms: &[u8; 32],
    view: &str,
    space: LayerSpace,
    level: u32,
    geometry: Geometry,
) -> String {
    let mut hasher = Sha256::new();
    hasher.update(b"counts");
    hasher.update(terms);
    level_of(&mut hasher, view, space, level);
    hasher.update([geometry as u8]);
    hex(&hasher.finalize())
}

/// What one level's denied-row labels in one view are a function of, as a file stem, the level
/// named as [`counts_stem`] names it.
pub(crate) fn denied_stem(view: &str, space: LayerSpace, level: u32) -> String {
    let mut hasher = Sha256::new();
    hasher.update(b"denied");
    level_of(&mut hasher, view, space, level);
    hex(&hasher.finalize())
}

fn level_of(hasher: &mut Sha256, view: &str, space: LayerSpace, level: u32) {
    hasher.update((view.len() as u64).to_le_bytes());
    hasher.update(view.as_bytes());
    hasher.update(space.raw().to_le_bytes());
    hasher.update(level.to_le_bytes());
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
    if !write_beside(dir, stem, at, extension, magic, payload) {
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

/// Write `payload` as the entry `stem` at `at`, beside the entry's other versions. `false` where
/// it could not be written.
fn write_beside(
    dir: &Path,
    stem: &str,
    at: u64,
    extension: &str,
    magic: &[u8; 8],
    payload: &[u8],
) -> bool {
    let mut bytes = Vec::with_capacity(HEADER + payload.len());
    bytes.extend_from_slice(magic);
    bytes.extend_from_slice(&(payload.len() as u64).to_le_bytes());
    bytes.extend_from_slice(&Sha256::digest(payload));
    bytes.extend_from_slice(payload);
    let path = path_of(dir, stem, at, extension);
    if let Err(error) = mosaica_authz::write_private_atomically(&path, &bytes) {
        tracing::warn!(
            path = %path.display(),
            %error,
            "a level's figures could not be written to the cache directory; a restart walks them again"
        );
        return false;
    }
    true
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

/// Little-endian reads off the front of a payload. `mosaica-store`'s readers are private to the
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
    fn u128(&mut self) -> Option<u128> {
        Some(u128::from_le_bytes(self.take(16)?.try_into().ok()?))
    }
    fn identity(&mut self) -> Option<[u8; 32]> {
        self.take(32)?.try_into().ok()
    }
    fn wide(&mut self) -> Option<Wide> {
        Some(Wide {
            count: self.u64()?,
            placed: self.u64()?,
            sums: [self.u128()?, self.u128()?],
        })
    }
    fn len(&mut self) -> Option<usize> {
        usize::try_from(self.u64()?).ok()
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

fn put_len(payload: &mut Vec<u8>, len: usize) {
    payload.extend_from_slice(&(len as u64).to_le_bytes());
}

fn put_wide(payload: &mut Vec<u8>, wide: &Wide) {
    put_u64s(payload, &[wide.count, wide.placed]);
    payload.extend_from_slice(&wide.sums[0].to_le_bytes());
    payload.extend_from_slice(&wide.sums[1].to_le_bytes());
}

/// The geometry a sum or a part holds, as one byte: positions, and a reserve beside them.
fn flags(positions: bool, reserves: bool) -> u8 {
    u8::from(positions) | (u8::from(reserves) << 1)
}

/// Whether a sum's `flags` hold what `geometry` asks for: positions with a centroid or a box, and
/// a reserve only with a box, which an entry too large for the bound goes without.
fn holds(flags: u8, geometry: Geometry) -> Option<(bool, bool)> {
    let (positions, reserves) = (flags & 1 != 0, flags & 2 != 0);
    let wanted =
        positions == (geometry != Geometry::None) && (!reserves || geometry == Geometry::Box);
    wanted.then_some((positions, reserves))
}

/// Write a level's sum for one grant: its sides, the walks' sum, what each side's growth since its
/// walk added, and the reserve.
pub(crate) fn write_sum(dir: &Path, stem: &str, counts: &CountsAt, reserves: Option<&Reserves>) {
    let dense = &counts.dense;
    let n = dense.counts.len();
    let positions = dense.positions();
    let mut payload = Vec::with_capacity(n * if positions { 40 } else { 4 } + 64);
    put_len(&mut payload, counts.sides.len());
    for side in counts.sides.iter() {
        payload.extend_from_slice(&side.shard.raw().to_le_bytes());
        payload.extend_from_slice(&side.identity);
        put_u64s(&mut payload, &[side.filled_at, side.at]);
        let mut grown: Vec<(&u32, &SideGrown)> = side.grown.iter().collect();
        grown.sort_unstable_by_key(|(ordinal, _)| **ordinal);
        put_len(&mut payload, grown.len());
        for (ordinal, g) in grown {
            put_u32s(&mut payload, &[*ordinal, g.count, g.placed]);
            put_u64s(&mut payload, &g.sums);
        }
    }
    put_len(&mut payload, n);
    payload.push(flags(positions, reserves.is_some()));
    put_u32s(&mut payload, &dense.counts);
    if positions {
        put_u32s(&mut payload, &dense.placed);
        put_u64s(&mut payload, dense.sums.as_flattened());
        put_u32s(&mut payload, dense.boxes.as_flattened());
    }
    let mut wide: Vec<(&u32, &Wide)> = dense.wide.iter().collect();
    wide.sort_unstable_by_key(|(ordinal, _)| **ordinal);
    put_len(&mut payload, wide.len());
    for (ordinal, w) in wide {
        put_u32s(&mut payload, &[*ordinal]);
        put_wide(&mut payload, w);
    }
    let mut grown: Vec<(&u32, &Grown)> = counts.grown.iter().collect();
    grown.sort_unstable_by_key(|(ordinal, _)| **ordinal);
    put_len(&mut payload, grown.len());
    for (ordinal, g) in grown {
        put_u32s(&mut payload, &[*ordinal]);
        put_wide(&mut payload, &g.figures);
        put_len(&mut payload, g.points.len());
        for &(shard, row, x, y) in &g.points {
            put_u32s(&mut payload, &[shard, row, x, y]);
        }
    }
    if let Some(reserves) = reserves {
        put_len(&mut payload, reserves.index.len());
        put_u32s(&mut payload, &reserves.index);
        payload.extend_from_slice(&reserves.alone.to_le_bytes());
        put_len(&mut payload, reserves.rows.len());
        put_len(&mut payload, reserves.shards.len());
        for rows in reserves.rows.iter().chain(&reserves.shards) {
            put_u32s(&mut payload, rows.as_flattened());
        }
    }
    write(dir, stem, 0, "sum", SUM_MAGIC, &payload);
}

/// A level's sum for one grant, and its reserve, where it is on disk with the geometry `geometry`
/// asks for.
pub(crate) fn read_sum(
    dir: &Path,
    stem: &str,
    geometry: Geometry,
) -> Option<(CountsAt, Option<Reserves>)> {
    let payload = read(dir, stem, 0, "sum", SUM_MAGIC)?;
    let mut r = Reader(&payload);
    let mut sides = Vec::new();
    for _ in 0..r.len()? {
        let shard = mosaica_types::ShardId::new(r.u32()?).ok()?;
        let identity = r.identity()?;
        let (filled_at, at) = (r.u64()?, r.u64()?);
        let mut grown = FxHashMap::default();
        for _ in 0..r.len()? {
            let head = r.u32s(3)?;
            let sums = r.u64s(2)?;
            grown.insert(
                head[0],
                SideGrown {
                    count: head[1],
                    placed: head[2],
                    sums: [sums[0], sums[1]],
                },
            );
        }
        sides.push(SideAt {
            shard,
            identity,
            filled_at,
            at,
            grown: std::sync::Arc::new(grown),
        });
    }
    let n = r.len()?;
    let (positions, reserved) = holds(r.u8()?, geometry)?;
    let mut dense = Dense {
        counts: r.u32s(n)?,
        ..Dense::default()
    };
    if positions {
        dense.placed = r.u32s(n)?;
        dense.sums = r.u64s(2 * n)?.as_chunks::<2>().0.to_vec();
        dense.boxes = r.u32s(4 * n)?.as_chunks::<4>().0.to_vec();
    }
    for _ in 0..r.len()? {
        let ordinal = r.u32()?;
        dense.wide.insert(ordinal, r.wide()?);
    }
    let mut grown = FxHashMap::default();
    for _ in 0..r.len()? {
        let ordinal = r.u32()?;
        let figures = r.wide()?;
        let points = r.len()?;
        let flat = r.u32s(points.checked_mul(4)?)?;
        grown.insert(
            ordinal,
            Grown {
                figures,
                points: flat
                    .as_chunks::<4>()
                    .0
                    .iter()
                    .map(|p| (p[0], p[1], p[2], p[3]))
                    .collect(),
            },
        );
    }
    let reserves = match reserved {
        false => None,
        true => {
            let len = r.len()?;
            let index = r.u32s(len)?;
            let alone = r.u32()?;
            let (held, tagged) = (r.len()?, r.len()?);
            let mut slots = |n: usize| -> Option<Vec<[[u32; RESERVE]; 4]>> {
                let flat = r.u32s(n.checked_mul(4 * RESERVE)?)?;
                Some(flat.as_chunks::<RESERVE>().0.as_chunks::<4>().0.to_vec())
            };
            let (rows, shards) = (slots(held)?, slots(tagged)?);
            if !(shards.is_empty() || shards.len() == rows.len()) {
                return None;
            }
            Some(Reserves {
                index,
                rows,
                shards,
                alone,
            })
        }
    };
    if !r.0.is_empty() {
        return None;
    }
    let counts = CountsAt {
        sides: sides.into(),
        dense: std::sync::Arc::new(dense),
        grown: std::sync::Arc::new(grown),
    };
    Some((counts, reserves))
}

/// The flags of a walk taken for `geometry`: positions with a centroid or a box, and a reserve
/// with a box.
fn walked_for(geometry: Geometry) -> (bool, bool) {
    (geometry != Geometry::None, geometry == Geometry::Box)
}

/// Write one side's walk of a level for `geometry`, filed under the level version it read, beside
/// its walks at other versions, which a sum held may still name.
pub(crate) fn write_part(dir: &Path, stem: &str, filled_at: u64, part: &Part, geometry: Geometry) {
    let k = part.ordinals.len();
    let (positions, reserves) = walked_for(geometry);
    let mut payload = Vec::with_capacity(k * if positions { 172 } else { 8 } + 32);
    payload.extend_from_slice(&part.len.to_le_bytes());
    payload.push(flags(positions, reserves));
    put_len(&mut payload, k);
    put_u32s(&mut payload, &part.ordinals);
    put_u32s(&mut payload, &part.counts);
    if positions {
        put_u32s(&mut payload, &part.placed);
        put_u64s(&mut payload, part.sums.as_flattened());
        put_u32s(&mut payload, part.boxes.as_flattened());
    }
    if reserves {
        for rows in &part.reserves {
            put_u32s(&mut payload, rows.as_flattened());
        }
    }
    write_beside(dir, stem, filled_at, "part", PART_MAGIC, &payload);
}

/// One side's walk of a level at `filled_at`, where it is on disk with the geometry `geometry`
/// asks for.
pub(crate) fn read_part(
    dir: &Path,
    stem: &str,
    filled_at: u64,
    geometry: Geometry,
) -> Option<Part> {
    let payload = read(dir, stem, filled_at, "part", PART_MAGIC)?;
    let mut r = Reader(&payload);
    let len = r.u32()?;
    let flags = r.u8()?;
    let k = r.len()?;
    let ordinals = r.u32s(k)?;
    let counts = r.u32s(k)?;
    let (positions, reserved) = walked_for(geometry);
    if flags != self::flags(positions, reserved) {
        return None;
    }
    let mut part = Part {
        len,
        ordinals,
        counts,
        ..Part::default()
    };
    if positions {
        part.placed = r.u32s(k)?;
        part.sums = r.u64s(2 * k)?.as_chunks::<2>().0.to_vec();
        part.boxes = r.u32s(4 * k)?.as_chunks::<4>().0.to_vec();
    }
    if reserved {
        let flat = r.u32s(k.checked_mul(4 * RESERVE)?)?;
        part.reserves = flat.as_chunks::<RESERVE>().0.as_chunks::<4>().0.to_vec();
    }
    let framed = r.0.is_empty() && part.ordinals.windows(2).all(|w| w[0] < w[1]);
    framed.then_some(part)
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
