//! **A row-major level's members by artifact: each artifact's base rows as a bitmap, and a covering
//! of at most a few row ranges that holds them all.**
//!
//! A row-major column answers *which artifact does this row carry*. This file answers the transpose
//! of that, *which rows does this artifact hold*, for the same base rows, and beside it a coarse
//! answer to *where could this artifact be*: at most [`crate::derived::COVERING_RANGES`] inclusive
//! row ranges whose union holds every member. Both are functions of the column's labels alone and
//! are written from them by [`crate::derived::project_row_members`], whenever and wherever the
//! column is written.
//!
//! # The format
//!
//! ```text
//! header   := magic "TSRM" | u16 version | u16 ranges a covering holds at most
//!             | u32 ordinals | u32 rows | u64 ranges | u64 payload_at | u64 ranges_at
//!             | u64 file_len | zero padding to 64 bytes
//! table    := ordinals × (u64 offset into the payload | u32 length | u32 members)
//! cover_at := (ordinals + 1) × u64 index into `ranges`, ascending, the last equal to `ranges`
//! payload  := at `payload_at`, a multiple of 32: one CRoaring frozen bitmap per artifact with
//!             members, each at an offset that is a multiple of 32; none for an artifact without
//! ranges   := at `ranges_at`: `ranges` × (u32 lo | u32 hi), inclusive, ascending and disjoint
//!             within each artifact
//! ```
//!
//! A hole and an artifact that labels no row are the same here, an entry of length zero with no
//! ranges, and the records tell them apart, as they do for the column.
//!
//! # What the open checks
//!
//! The header, the table and the ranges are checked whole: offsets aligned, ascending and inside the
//! payload, every range inside the rows and in order, no covering longer than the header allows. The
//! payload is not read. A frozen view over bytes that are not a frozen bitmap is undefined, so that
//! the payload is what the writer produced rests on the bundle's digest sweep, as it does for
//! [`crate::term_images`], or, for a file this process composed in its own scratch, on having just
//! written it.

use std::fs::File;
use std::io::{BufWriter, Seek, SeekFrom, Write};
use std::os::unix::fs::FileExt;
use std::path::{Path, PathBuf};

use croaring::{Bitmap, BitmapView, Frozen};
use memmap2::Mmap;

use crate::error::{Result, StoreError};

const MAGIC: &[u8; 4] = b"TSRM";
const VERSION: u16 = 1;
const HEADER_LEN: usize = 64;
const ENTRY_LEN: usize = 16;
/// CRoaring's frozen deserialiser needs 32-byte alignment, and a mapping is page-aligned.
const ALIGN: u64 = 32;

fn malformed(path: &Path, detail: impl std::fmt::Display) -> StoreError {
    StoreError::MalformedBundle {
        detail: format!("row member file {}: {detail}", path.display()),
    }
}

fn io(path: &Path) -> impl Fn(std::io::Error) -> StoreError + '_ {
    move |source| StoreError::Io {
        path: path.to_path_buf(),
        source,
    }
}

fn align_up(at: u64) -> u64 {
    at.div_ceil(ALIGN) * ALIGN
}

fn le_u32(bytes: &[u8], at: usize) -> u32 {
    u32::from_le_bytes(bytes[at..at + 4].try_into().expect("four bytes"))
}

fn le_u64(bytes: &[u8], at: usize) -> u64 {
    u64::from_le_bytes(bytes[at..at + 8].try_into().expect("eight bytes"))
}

/// Where the table and `cover_at` end, which is where the payload may begin.
fn tables_end(ordinals: u32) -> u64 {
    HEADER_LEN as u64 + ENTRY_LEN as u64 * u64::from(ordinals) + 8 * (u64::from(ordinals) + 1)
}

/// Writes one member file front to back: the payload as each artifact arrives, then the ranges, the
/// table and the header once every artifact has.
pub struct RowMembersFile {
    out: BufWriter<File>,
    path: PathBuf,
    ordinals: u32,
    rows: u32,
    max_ranges: u16,
    /// Per ordinal: offset, length, members.
    table: Vec<(u64, u32, u32)>,
    cover_at: Vec<u64>,
    ranges: Vec<(u32, u32)>,
    payload_at: u64,
    /// Bytes of payload written so far.
    written: u64,
    next: u32,
    frozen: Vec<u8>,
}

impl RowMembersFile {
    pub fn create(path: &Path, ordinals: u32, rows: u32, max_ranges: u16) -> Result<Self> {
        let file = File::create(path).map_err(io(path))?;
        let payload_at = align_up(tables_end(ordinals));
        let mut out = BufWriter::with_capacity(1 << 20, file);
        out.seek(SeekFrom::Start(payload_at)).map_err(io(path))?;
        Ok(RowMembersFile {
            out,
            path: path.to_path_buf(),
            ordinals,
            rows,
            max_ranges,
            table: vec![(0, 0, 0); ordinals as usize],
            cover_at: Vec::with_capacity(ordinals as usize + 1),
            ranges: Vec::new(),
            payload_at,
            written: 0,
            next: 0,
            frozen: Vec::new(),
        })
    }

    /// One artifact's members and covering. Ordinals arrive ascending; one skipped has none.
    pub fn push(&mut self, ordinal: u32, members: &Bitmap, covering: &[(u32, u32)]) -> Result<()> {
        assert!(
            ordinal >= self.next && ordinal < self.ordinals,
            "a member file takes each ordinal of its level once, ascending"
        );
        self.skip_to(ordinal);
        self.cover_at.push(self.ranges.len() as u64);
        self.ranges.extend_from_slice(covering);
        self.next = ordinal + 1;
        if members.is_empty() {
            return Ok(());
        }
        let at = align_up(self.written);
        if at > self.written {
            let pad = [0u8; ALIGN as usize];
            self.out
                .write_all(&pad[..(at - self.written) as usize])
                .map_err(io(&self.path))?;
        }
        self.frozen.clear();
        let bytes = members.serialize_into_vec::<Frozen>(&mut self.frozen);
        self.out.write_all(bytes).map_err(io(&self.path))?;
        let len = bytes.len() as u32;
        self.table[ordinal as usize] = (at, len, members.cardinality() as u32);
        self.written = at + u64::from(len);
        Ok(())
    }

    fn skip_to(&mut self, ordinal: u32) {
        while self.next < ordinal {
            self.cover_at.push(self.ranges.len() as u64);
            self.next += 1;
        }
    }

    /// Write the ranges, the table and the header. The file is then the level's member file.
    pub fn finish(mut self) -> Result<()> {
        self.skip_to(self.ordinals);
        self.cover_at.push(self.ranges.len() as u64);
        let ranges_at = self.payload_at + align_up(self.written);
        let pad = (ranges_at - self.payload_at - self.written) as usize;
        self.out
            .write_all(&vec![0u8; pad])
            .map_err(io(&self.path))?;
        for (lo, hi) in &self.ranges {
            self.out
                .write_all(&lo.to_le_bytes())
                .map_err(io(&self.path))?;
            self.out
                .write_all(&hi.to_le_bytes())
                .map_err(io(&self.path))?;
        }
        let file_len = ranges_at + 8 * self.ranges.len() as u64;
        let file = self.out.into_inner().map_err(|e| StoreError::Io {
            path: self.path.clone(),
            source: e.into_error(),
        })?;
        let mut head = Vec::with_capacity(self.payload_at as usize);
        head.extend_from_slice(MAGIC);
        head.extend_from_slice(&VERSION.to_le_bytes());
        head.extend_from_slice(&self.max_ranges.to_le_bytes());
        head.extend_from_slice(&self.ordinals.to_le_bytes());
        head.extend_from_slice(&self.rows.to_le_bytes());
        head.extend_from_slice(&(self.ranges.len() as u64).to_le_bytes());
        head.extend_from_slice(&self.payload_at.to_le_bytes());
        head.extend_from_slice(&ranges_at.to_le_bytes());
        head.extend_from_slice(&file_len.to_le_bytes());
        head.resize(HEADER_LEN, 0);
        for (offset, len, members) in &self.table {
            head.extend_from_slice(&offset.to_le_bytes());
            head.extend_from_slice(&len.to_le_bytes());
            head.extend_from_slice(&members.to_le_bytes());
        }
        for at in &self.cover_at {
            head.extend_from_slice(&at.to_le_bytes());
        }
        head.resize(self.payload_at as usize, 0);
        file.write_all_at(&head, 0).map_err(io(&self.path))?;
        file.set_len(file_len).map_err(io(&self.path))?;
        Ok(())
    }
}

/// One member file, mapped and checked. Bitmaps are frozen views into the mapping, so nothing is
/// copied until a caller copies it.
pub struct RowMembersPack {
    map: Mmap,
    ordinals: u32,
    rows: u32,
    max_ranges: u16,
    ranges: u64,
    payload_at: usize,
    ranges_at: usize,
}

impl std::fmt::Debug for RowMembersPack {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("RowMembersPack")
            .field("ordinals", &self.ordinals)
            .field("rows", &self.rows)
            .field("ranges", &self.ranges)
            .finish()
    }
}

impl RowMembersPack {
    pub fn open(path: &Path) -> Result<Self> {
        let file = File::open(path).map_err(io(path))?;
        // SAFETY: opened read-only and never written through. A bundle's derived file is never
        // truncated while named (its name carries the publication that wrote it), and a scratch
        // file is unlinked by the process that wrote it once mapped, which leaves the mapping valid.
        let map = unsafe { Mmap::map(&file) }.map_err(io(path))?;
        if map.len() < HEADER_LEN {
            return Err(malformed(
                path,
                format!("{} bytes is shorter than the header", map.len()),
            ));
        }
        if &map[0..4] != MAGIC {
            return Err(malformed(path, "the magic is not TSRM"));
        }
        let version = u16::from_le_bytes([map[4], map[5]]);
        if version != VERSION {
            return Err(malformed(
                path,
                format!("version {version}, expected {VERSION}"),
            ));
        }
        let max_ranges = u16::from_le_bytes([map[6], map[7]]);
        let ordinals = le_u32(&map, 8);
        let rows = le_u32(&map, 12);
        let ranges = le_u64(&map, 16);
        let payload_at = le_u64(&map, 24);
        let ranges_at = le_u64(&map, 32);
        let file_len = le_u64(&map, 40);
        if file_len != map.len() as u64 {
            return Err(malformed(
                path,
                format!(
                    "the header says {file_len} bytes and the file is {}",
                    map.len()
                ),
            ));
        }
        if payload_at != align_up(tables_end(ordinals))
            || ranges_at < payload_at
            || !ranges_at.is_multiple_of(ALIGN)
            || ranges.checked_mul(8).and_then(|b| b.checked_add(ranges_at)) != Some(file_len)
        {
            return Err(malformed(
                path,
                "the header's sections do not tile the file",
            ));
        }
        let pack = RowMembersPack {
            map,
            ordinals,
            rows,
            max_ranges,
            ranges,
            payload_at: payload_at as usize,
            ranges_at: ranges_at as usize,
        };
        let payload_len = (ranges_at - payload_at) as usize;
        let mut end = 0usize;
        for ordinal in 0..ordinals {
            let (offset, len, members) = pack.entry(ordinal);
            if len == 0 {
                if members != 0 {
                    return Err(malformed(
                        path,
                        format!("ordinal {ordinal} counts members and has no bitmap"),
                    ));
                }
                continue;
            }
            if !offset.is_multiple_of(ALIGN as usize) || offset < end || offset + len > payload_len {
                return Err(malformed(
                    path,
                    format!(
                        "ordinal {ordinal}'s bitmap is misaligned, overlaps another or leaves the \
                         payload"
                    ),
                ));
            }
            end = offset + len;
        }
        let mut previous = 0u64;
        for ordinal in 0..=ordinals {
            let at = pack.cover_at(ordinal);
            if at < previous || at > ranges {
                return Err(malformed(
                    path,
                    format!("ordinal {ordinal}'s covering starts out of order"),
                ));
            }
            if ordinal > 0 && at - previous > u64::from(max_ranges) {
                return Err(malformed(
                    path,
                    format!(
                        "ordinal {}'s covering holds more than {max_ranges} ranges",
                        ordinal - 1
                    ),
                ));
            }
            previous = at;
        }
        if previous != ranges {
            return Err(malformed(
                path,
                "the coverings do not account for every range",
            ));
        }
        for ordinal in 0..ordinals {
            let mut last: Option<u32> = None;
            for (lo, hi) in pack.covering(ordinal) {
                if lo > hi || hi >= rows || last.is_some_and(|last| lo <= last) {
                    return Err(malformed(
                        path,
                        format!(
                            "ordinal {ordinal}'s covering is not ascending, disjoint ranges \
                             inside the rows"
                        ),
                    ));
                }
                last = Some(hi);
            }
        }
        Ok(pack)
    }

    fn entry(&self, ordinal: u32) -> (usize, usize, u32) {
        let at = HEADER_LEN + ENTRY_LEN * ordinal as usize;
        (
            le_u64(&self.map, at) as usize,
            le_u32(&self.map, at + 8) as usize,
            le_u32(&self.map, at + 12),
        )
    }

    fn cover_at(&self, ordinal: u32) -> u64 {
        le_u64(
            &self.map,
            HEADER_LEN + ENTRY_LEN * self.ordinals as usize + 8 * ordinal as usize,
        )
    }

    /// How many ordinals the file covers, holes included.
    pub fn ordinals(&self) -> u32 {
        self.ordinals
    }

    /// The base rows the members are taken over.
    pub fn rows(&self) -> u32 {
        self.rows
    }

    /// The most ranges one covering holds.
    pub fn max_ranges(&self) -> usize {
        usize::from(self.max_ranges)
    }

    /// Every range of every covering.
    pub fn total_ranges(&self) -> u64 {
        self.ranges
    }

    /// The artifact's members, or `None` where it has none or is past the file.
    pub fn members(&self, ordinal: u32) -> Option<BitmapView<'_>> {
        if ordinal >= self.ordinals {
            return None;
        }
        let (offset, len, _) = self.entry(ordinal);
        if len == 0 {
            return None;
        }
        let from = self.payload_at + offset;
        // SAFETY: the open proved the range aligned and inside the payload, and the payload is what
        // the writer serialised there (module doc).
        Some(unsafe { BitmapView::deserialize::<Frozen>(&self.map[from..from + len]) })
    }

    /// How many members the artifact has.
    pub fn member_count(&self, ordinal: u32) -> u32 {
        if ordinal >= self.ordinals {
            return 0;
        }
        self.entry(ordinal).2
    }

    /// The artifact's covering, ascending. Empty for an artifact with no members.
    pub fn covering(&self, ordinal: u32) -> impl Iterator<Item = (u32, u32)> + '_ {
        let (from, to) = if ordinal < self.ordinals {
            (self.cover_at(ordinal), self.cover_at(ordinal + 1))
        } else {
            (0, 0)
        };
        (from..to).map(move |i| {
            let at = self.ranges_at + 8 * i as usize;
            (le_u32(&self.map, at), le_u32(&self.map, at + 4))
        })
    }

    /// The file's size on disk.
    pub fn file_len(&self) -> usize {
        self.map.len()
    }

    /// The bytes the bitmaps take, padding included.
    pub fn payload_len(&self) -> usize {
        self.ranges_at - self.payload_at
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// One artifact as the writer takes it: ordinal, members, covering.
    type Artifact<'a> = (u32, &'a [u32], &'a [(u32, u32)]);

    fn write(dir: &Path, levels: &[Artifact<'_>], ordinals: u32) -> PathBuf {
        let path = dir.join("m.tsrm");
        let mut file = RowMembersFile::create(&path, ordinals, 1000, 4).unwrap();
        for (ordinal, members, covering) in levels {
            file.push(*ordinal, &Bitmap::of(members), covering).unwrap();
        }
        file.finish().unwrap();
        path
    }

    #[test]
    fn what_is_written_reads_back_and_a_gap_is_an_artifact_without_members() {
        let dir = tempfile::tempdir().unwrap();
        let path = write(
            dir.path(),
            &[
                (0, &[1, 2, 3], &[(1, 3)]),
                (2, &[10, 500], &[(10, 10), (500, 500)]),
            ],
            4,
        );
        let pack = RowMembersPack::open(&path).unwrap();
        assert_eq!(pack.ordinals(), 4);
        assert_eq!(pack.members(0).unwrap().to_bitmap(), Bitmap::of(&[1, 2, 3]));
        assert!(pack.members(1).is_none());
        assert_eq!(pack.member_count(2), 2);
        assert_eq!(
            pack.covering(2).collect::<Vec<_>>(),
            vec![(10, 10), (500, 500)]
        );
        assert!(pack.members(3).is_none());
        assert_eq!(pack.covering(3).count(), 0);
        assert!(pack.members(9).is_none());
    }

    #[test]
    fn a_truncated_or_altered_file_is_refused() {
        let dir = tempfile::tempdir().unwrap();
        let path = write(dir.path(), &[(0, &[1, 2, 3], &[(1, 3)])], 1);
        let bytes = std::fs::read(&path).unwrap();
        for cut in [0, 10, HEADER_LEN, bytes.len() - 1] {
            std::fs::write(&path, &bytes[..cut]).unwrap();
            assert!(RowMembersPack::open(&path).is_err(), "cut at {cut}");
        }
        let mut swapped = bytes.clone();
        let last = swapped.len() - 4;
        swapped[last..].copy_from_slice(&5000u32.to_le_bytes());
        std::fs::write(&path, &swapped).unwrap();
        assert!(
            RowMembersPack::open(&path).is_err(),
            "a range past the rows"
        );
    }
}
