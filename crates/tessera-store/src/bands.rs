//! A segment's identity bands, the copies beside them, and its cell codes.
//!
//! **Band `j`** holds every row of a segment whose `tessera_id` has at least `j` leading zero bits.
//! Identities are a keyed permutation of entity ids, so band `j` holds about `2^-j` of the rows, and
//! the `m` smallest identities of a tile that lie below a cut are read from the narrowest band that
//! still holds them rather than from the identity column. Bands [`FIRST_BAND`] and up are written,
//! every one of them that holds a row. They nest: band `j + 1` is a subset of band `j`, so the file
//! holds about twice band [`FIRST_BAND`], `n / 32` entries.
//!
//! An **entry** is `(row, tessera_id, morton, residual)` for one row, and a band's entries are in
//! row order, which is also Morton order: a tile is a contiguous run of a band, found by binary
//! search on its codes. Beside the entries, every render column of the segment is copied in entry
//! order, so a band entry's drawn value is read beside it instead of from the column at a scattered
//! row. Each layer level's per-row label is copied the same way, but by the level's own writer
//! ([`write_band_labels`]), since a level's column is written after its segment and moves with the
//! level rather than with the rows.
//!
//! # `bands.bin`
//!
//! One file per segment, little-endian:
//!
//! ```text
//! header   64 B   magic "TSBD", version u16, first band u16, band count u32 (B), row count u32,
//!                 entries u64 (T), copy count u32 (C), zero to the end
//! starts   (B + 1) x u64   entry index at which each band begins; starts[B] == T
//! copies   C x 8 B         per copy: name length u16, type tag u8, width u8, zero u32
//! names    the copies' names, UTF-8, concatenated in table order
//! -- each section below begins on a 4096-byte boundary --
//! ids        T x u64
//! rows       T x u32
//! codes      T x u32
//! residuals  T x u32
//! copy k     T x width_k
//! ```
//!
//! Entry `e` is `(rows[e], ids[e], codes[e], residuals[e])`, and band `j`'s entries are
//! `starts[j - first] .. starts[j - first + 1]`. A copy holds entry `e`'s value at index `e`.
//!
//! **One file with an index, sections apart.** Each section is one typed array at its natural
//! alignment, so a reader takes `&[u64]` and `&[u32]` straight off the mapping with no per-entry
//! decode. Selection reads rows and identities for every candidate and codes, residuals and copies
//! only for the entries it serves, so splitting the fields keeps the scanned bytes at 12 an entry
//! rather than the 20 a packed entry costs. Page-aligned sections can be mapped or advised one at a
//! time, so a reader can give its scattered reads `MADV_RANDOM` on that section's pages without
//! changing how the rest of the file is read. The cost is under a page per section.
//!
//! # `cell-codes.u32`
//!
//! The Morton code of each occupied cell, one `u32` per entry of `cuts.u32` and in the same order:
//! `cell_codes[i] == morton[cuts[i]]`. Tile ranges and occupied-cell counts are answered from it and
//! `cuts.u32` without reading `morton.u32`. [`crate::write::CutWriter`] writes both files, since it
//! is where a cell's first row is seen.
//!
//! # One writer
//!
//! Every producer of a segment feeds [`BandWriter`] its rows in row order and calls
//! [`BandWriter::finish`] once `columns.arrow` is complete: [`crate::write::SegmentWriter`] does
//! so for a flush, a merge, the compaction fold and the in-memory build, and the bounded build
//! assembly does so directly. The copies are read back from the finished `columns.arrow`, so they
//! are the stored column's values by construction whichever producer wrote it.
//!
//! # What the bands do not hold
//!
//! **No visibility.** A deleted or suppressed row stays in its segment's bands until a compaction
//! rewrites the segment, as it stays in the segment's columns; the composed mask, intersected with
//! a band's rows, is what removes it from an answer.

use std::fs::File;
use std::io::{self, BufWriter, Write};
use std::path::{Path, PathBuf};

use memmap2::{Mmap, MmapMut};

use crate::error::{Result, StoreError};
use crate::read::{ColumnsRef, ScalarSlice};

/// A segment's band file, beside `columns.arrow`.
pub const BANDS_FILE: &str = "bands.bin";

/// A segment's cell codes, beside `cuts.u32`.
pub const CELL_CODES_FILE: &str = "cell-codes.u32";

/// The widest band written. Band `j` holds about `2^-j` of a segment's rows, and below this the
/// band is too wide for the identity column's own scan to lose to it.
pub const FIRST_BAND: u32 = 6;

const MAGIC: &[u8; 4] = b"TSBD";
const VERSION: u16 = 1;
const HEADER_BYTES: usize = 64;
const COPY_RECORD_BYTES: usize = 8;
const SECTION_ALIGN: usize = 4096;
/// `(id u64, row u32, code u32, residual u32)` in the spool [`BandWriter`] streams through.
const SPOOL_RECORD_BYTES: usize = 20;

/// The band a row falls in at most: its identity's leading zero bits.
pub fn band_of(tessera_id: u64) -> u32 {
    tessera_id.leading_zeros()
}

/// A stored type a copy can hold, at the width it is stored at. `Bool` is one byte, 0 or 1.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CopyType {
    Bool,
    U8,
    U16,
    U32,
    U64,
    I8,
    I16,
    I32,
    I64,
    F32,
    F64,
    TimestampUs,
}

impl CopyType {
    const ALL: [CopyType; 12] = [
        CopyType::Bool,
        CopyType::U8,
        CopyType::U16,
        CopyType::U32,
        CopyType::U64,
        CopyType::I8,
        CopyType::I16,
        CopyType::I32,
        CopyType::I64,
        CopyType::F32,
        CopyType::F64,
        CopyType::TimestampUs,
    ];

    fn tag(self) -> u8 {
        CopyType::ALL
            .iter()
            .position(|t| *t == self)
            .expect("every type is in ALL") as u8
    }

    fn of_tag(tag: u8) -> Option<Self> {
        CopyType::ALL.get(usize::from(tag)).copied()
    }

    pub fn width(self) -> usize {
        match self {
            CopyType::Bool | CopyType::U8 | CopyType::I8 => 1,
            CopyType::U16 | CopyType::I16 => 2,
            CopyType::U32 | CopyType::I32 | CopyType::F32 => 4,
            CopyType::U64 | CopyType::I64 | CopyType::F64 | CopyType::TimestampUs => 8,
        }
    }

    /// The copy type of a stored column, or `None` for a variable-width one, which no render
    /// column is.
    fn of_slice(slice: &ScalarSlice<'_>) -> Option<Self> {
        Some(match slice {
            ScalarSlice::Bool(_) => CopyType::Bool,
            ScalarSlice::U8(_) => CopyType::U8,
            ScalarSlice::U16(_) => CopyType::U16,
            ScalarSlice::U32(_) => CopyType::U32,
            ScalarSlice::U64(_) => CopyType::U64,
            ScalarSlice::I8(_) => CopyType::I8,
            ScalarSlice::I16(_) => CopyType::I16,
            ScalarSlice::I32(_) => CopyType::I32,
            ScalarSlice::I64(_) => CopyType::I64,
            ScalarSlice::F32(_) => CopyType::F32,
            ScalarSlice::F64(_) => CopyType::F64,
            ScalarSlice::TimestampUs(_) => CopyType::TimestampUs,
            ScalarSlice::Utf8(_) => return None,
        })
    }
}

/// Write `row`'s value of `slice` into `out`, little-endian at the copy's width.
fn put_value(slice: &ScalarSlice<'_>, row: usize, out: &mut [u8]) {
    match slice {
        ScalarSlice::Bool(a) => out[0] = u8::from(a.value(row)),
        ScalarSlice::U8(v) => out.copy_from_slice(&v[row].to_le_bytes()),
        ScalarSlice::U16(v) => out.copy_from_slice(&v[row].to_le_bytes()),
        ScalarSlice::U32(v) => out.copy_from_slice(&v[row].to_le_bytes()),
        ScalarSlice::U64(v) => out.copy_from_slice(&v[row].to_le_bytes()),
        ScalarSlice::I8(v) => out.copy_from_slice(&v[row].to_le_bytes()),
        ScalarSlice::I16(v) => out.copy_from_slice(&v[row].to_le_bytes()),
        ScalarSlice::I32(v) => out.copy_from_slice(&v[row].to_le_bytes()),
        ScalarSlice::I64(v) => out.copy_from_slice(&v[row].to_le_bytes()),
        ScalarSlice::F32(v) => out.copy_from_slice(&v[row].to_le_bytes()),
        ScalarSlice::F64(v) => out.copy_from_slice(&v[row].to_le_bytes()),
        ScalarSlice::TimestampUs(v) => out.copy_from_slice(&v[row].to_le_bytes()),
        ScalarSlice::Utf8(_) => unreachable!("a variable-width column has no copy"),
    }
}

fn align_up(at: usize) -> usize {
    at.div_ceil(SECTION_ALIGN) * SECTION_ALIGN
}

/// Where everything in a band file begins, derived from its counts alone.
#[derive(Debug, Clone, PartialEq, Eq)]
struct Layout {
    band_count: usize,
    entries: usize,
    /// Per copy, where its values begin.
    copies: Vec<usize>,
    ids: usize,
    rows: usize,
    codes: usize,
    residuals: usize,
    len: usize,
}

impl Layout {
    fn of(band_count: usize, entries: usize, names_len: usize, widths: &[usize]) -> Self {
        let table = HEADER_BYTES + (band_count + 1) * 8 + widths.len() * COPY_RECORD_BYTES;
        let ids = align_up(table + names_len);
        let rows = align_up(ids + entries * 8);
        let codes = align_up(rows + entries * 4);
        let residuals = align_up(codes + entries * 4);
        let mut at = align_up(residuals + entries * 4);
        let mut copies = Vec::with_capacity(widths.len());
        for width in widths {
            copies.push(at);
            at = align_up(at + entries * width);
        }
        Layout {
            band_count,
            entries,
            copies,
            ids,
            rows,
            codes,
            residuals,
            len: at,
        }
    }
}

/// Streams a segment's rows into its band file. Holds a 20-byte spool record per row in band
/// [`FIRST_BAND`] and the per-band counts; nothing else is held.
pub struct BandWriter {
    spool: BufWriter<File>,
    /// Removes the spool however the writer ends, finished, failed or dropped.
    spool_path: RemoveOnDrop,
    /// Entries by their exact leading-zero count, `FIRST_BAND..=64`.
    by_zeros: [u64; 65],
    row: u32,
}

impl BandWriter {
    /// Start a band file for the segment in `dir`, which must exist.
    pub fn create(dir: &Path) -> io::Result<Self> {
        let spool_path = dir.join(format!("{BANDS_FILE}.spool"));
        let file = std::fs::OpenOptions::new()
            .read(true)
            .write(true)
            .create(true)
            .truncate(true)
            .open(&spool_path)?;
        Ok(BandWriter {
            spool: BufWriter::with_capacity(1 << 20, file),
            spool_path: RemoveOnDrop(spool_path),
            by_zeros: [0; 65],
            row: 0,
        })
    }

    /// Take the next row, in the row order the segment is written in.
    pub fn push(&mut self, tessera_id: u64, morton: u32, residual: u32) -> io::Result<()> {
        let zeros = band_of(tessera_id);
        if zeros >= FIRST_BAND {
            let mut record = [0u8; SPOOL_RECORD_BYTES];
            record[0..8].copy_from_slice(&tessera_id.to_le_bytes());
            record[8..12].copy_from_slice(&self.row.to_le_bytes());
            record[12..16].copy_from_slice(&morton.to_le_bytes());
            record[16..20].copy_from_slice(&residual.to_le_bytes());
            self.spool.write_all(&record)?;
            self.by_zeros[zeros as usize] += 1;
        }
        self.row += 1;
        Ok(())
    }

    /// Write `bands.bin` into `dir`, copying every render column of the segment's finished
    /// `columns.arrow`. The spool is removed whether this succeeds or not.
    pub fn finish(self, dir: &Path) -> io::Result<()> {
        let BandWriter {
            spool,
            spool_path,
            by_zeros,
            row,
        } = self;
        let _guard = spool_path;
        let spool = spool.into_inner().map_err(io::IntoInnerError::into_error)?;
        let columns = ColumnsRef::load(&dir.join("columns.arrow")).map_err(io::Error::other)?;
        if columns.row_count() != row {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                format!(
                    "bands: columns.arrow holds {} rows and {row} were written to the bands",
                    columns.row_count()
                ),
            ));
        }

        // Bands `FIRST_BAND..=deepest`, nested: band `j` counts every entry with `j` or more zeros.
        let deepest = (FIRST_BAND as usize..=64).rev().find(|&z| by_zeros[z] > 0);
        let band_count = deepest.map_or(0, |d| d + 1 - FIRST_BAND as usize);
        let mut starts = Vec::with_capacity(band_count + 1);
        let mut at = 0u64;
        for band in 0..band_count {
            starts.push(at);
            at += by_zeros[FIRST_BAND as usize + band..].iter().sum::<u64>();
        }
        starts.push(at);
        let entries = usize::try_from(at).map_err(io::Error::other)?;

        // A variable-width column has no copy. No declaration can render one, and the writer
        // accepts one only because the column format admits it.
        let copies: Vec<(&str, CopyType, ScalarSlice<'_>)> = columns
            .scalar_names()
            .filter_map(|name| {
                let slice = columns.scalar(name).expect("a listed column is held");
                CopyType::of_slice(&slice).map(|ty| (name, ty, slice))
            })
            .collect();
        let names_len: usize = copies.iter().map(|(name, _, _)| name.len()).sum();
        let widths: Vec<usize> = copies.iter().map(|(_, ty, _)| ty.width()).collect();
        let layout = Layout::of(band_count, entries, names_len, &widths);

        let file = std::fs::OpenOptions::new()
            .read(true)
            .write(true)
            .create(true)
            .truncate(true)
            .open(dir.join(BANDS_FILE))?;
        file.set_len(layout.len as u64)?;
        // SAFETY: this process created and sized the file above and holds the only handle to it.
        let mut map = unsafe { MmapMut::map_mut(&file) }?;

        map[0..4].copy_from_slice(MAGIC);
        map[4..6].copy_from_slice(&VERSION.to_le_bytes());
        map[6..8].copy_from_slice(&(FIRST_BAND as u16).to_le_bytes());
        map[8..12].copy_from_slice(&(band_count as u32).to_le_bytes());
        map[12..16].copy_from_slice(&row.to_le_bytes());
        map[16..24].copy_from_slice(&(entries as u64).to_le_bytes());
        map[24..28].copy_from_slice(&(copies.len() as u32).to_le_bytes());
        let mut at = HEADER_BYTES;
        for start in &starts {
            map[at..at + 8].copy_from_slice(&start.to_le_bytes());
            at += 8;
        }
        for (name, ty, _) in &copies {
            let len = u16::try_from(name.len()).map_err(|_| {
                io::Error::new(
                    io::ErrorKind::InvalidInput,
                    format!("bands: a column name of {} bytes is too long", name.len()),
                )
            })?;
            map[at..at + 2].copy_from_slice(&len.to_le_bytes());
            map[at + 2] = ty.tag();
            map[at + 3] = ty.width() as u8;
            at += COPY_RECORD_BYTES;
        }
        for (name, _, _) in &copies {
            map[at..at + name.len()].copy_from_slice(name.as_bytes());
            at += name.len();
        }

        // Band `FIRST_BAND` is the spool in order; each narrower band is the one before it,
        // filtered.
        let put = |map: &mut MmapMut, e: usize, id: u64, row: u32, code: u32, residual: u32| {
            map[layout.ids + e * 8..layout.ids + e * 8 + 8].copy_from_slice(&id.to_le_bytes());
            map[layout.rows + e * 4..layout.rows + e * 4 + 4].copy_from_slice(&row.to_le_bytes());
            map[layout.codes + e * 4..layout.codes + e * 4 + 4]
                .copy_from_slice(&code.to_le_bytes());
            map[layout.residuals + e * 4..layout.residuals + e * 4 + 4]
                .copy_from_slice(&residual.to_le_bytes());
        };
        if band_count > 0 {
            // SAFETY: the spool is this writer's own file and is not written while mapped.
            let spooled = unsafe { Mmap::map(&spool) }?;
            for (e, record) in spooled
                .as_chunks::<SPOOL_RECORD_BYTES>()
                .0
                .iter()
                .enumerate()
            {
                let read = |at: usize| u32::from_le_bytes(record[at..at + 4].try_into().unwrap());
                let id = u64::from_le_bytes(record[0..8].try_into().unwrap());
                put(&mut map, e, id, read(8), read(12), read(16));
            }
            let read_u32 = |map: &MmapMut, base: usize, e: usize| {
                u32::from_le_bytes(map[base + e * 4..base + e * 4 + 4].try_into().unwrap())
            };
            for band in 1..band_count {
                let zeros = FIRST_BAND + band as u32;
                let mut e = starts[band] as usize;
                for from in starts[band - 1] as usize..starts[band] as usize {
                    let id = u64::from_le_bytes(
                        map[layout.ids + from * 8..layout.ids + from * 8 + 8]
                            .try_into()
                            .unwrap(),
                    );
                    if band_of(id) < zeros {
                        continue;
                    }
                    let (row, code, residual) = (
                        read_u32(&map, layout.rows, from),
                        read_u32(&map, layout.codes, from),
                        read_u32(&map, layout.residuals, from),
                    );
                    put(&mut map, e, id, row, code, residual);
                    e += 1;
                }
                debug_assert_eq!(e as u64, starts[band + 1]);
            }
        }

        for ((_, ty, slice), &base) in copies.iter().zip(&layout.copies) {
            let width = ty.width();
            for e in 0..entries {
                let row = u32::from_le_bytes(
                    map[layout.rows + e * 4..layout.rows + e * 4 + 4]
                        .try_into()
                        .unwrap(),
                ) as usize;
                put_value(
                    slice,
                    row,
                    &mut map[base + e * width..base + (e + 1) * width],
                );
            }
        }
        map.flush()?;
        Ok(())
    }
}

struct RemoveOnDrop(PathBuf);

impl Drop for RemoveOnDrop {
    fn drop(&mut self) {
        let _ = std::fs::remove_file(&self.0);
    }
}

fn malformed(path: &Path, detail: impl std::fmt::Display) -> StoreError {
    StoreError::MalformedBundle {
        detail: format!("{}: {detail}", path.display()),
    }
}

fn map_file(path: &Path) -> Result<Mmap> {
    let file = File::open(path).map_err(|source| StoreError::Io {
        path: path.to_path_buf(),
        source,
    })?;
    // SAFETY: read-only for the mapping's lifetime; nothing truncates a published segment's files
    // while a generation names them.
    unsafe { Mmap::map(&file) }.map_err(|source| StoreError::Io {
        path: path.to_path_buf(),
        source,
    })
}

/// One copy's name, type and values, as [`Bands`] reads it.
#[derive(Debug, Clone, Copy)]
pub struct BandCopy<'a> {
    pub name: &'a str,
    pub ty: CopyType,
    /// Entry `e`'s value is `bytes[e * width..(e + 1) * width]`, little-endian.
    pub bytes: &'a [u8],
}

impl BandCopy<'_> {
    /// Entry `e`'s value as a scalar of the copied column's type.
    pub fn value_at(&self, e: usize) -> tessera_types::scalar::ScalarValue {
        use tessera_types::scalar::ScalarValue as V;
        let w = self.ty.width();
        let b = &self.bytes[e * w..(e + 1) * w];
        match self.ty {
            CopyType::Bool => V::Bool(b[0] != 0),
            CopyType::U8 => V::U8(b[0]),
            CopyType::U16 => V::U16(u16::from_le_bytes(b.try_into().unwrap())),
            CopyType::U32 => V::U32(u32::from_le_bytes(b.try_into().unwrap())),
            CopyType::U64 => V::U64(u64::from_le_bytes(b.try_into().unwrap())),
            CopyType::I8 => V::I8(b[0] as i8),
            CopyType::I16 => V::I16(i16::from_le_bytes(b.try_into().unwrap())),
            CopyType::I32 => V::I32(i32::from_le_bytes(b.try_into().unwrap())),
            CopyType::I64 => V::I64(i64::from_le_bytes(b.try_into().unwrap())),
            CopyType::F32 => V::F32(f32::from_le_bytes(b.try_into().unwrap())),
            CopyType::F64 => V::F64(f64::from_le_bytes(b.try_into().unwrap())),
            CopyType::TimestampUs => V::TimestampUs(i64::from_le_bytes(b.try_into().unwrap())),
        }
    }
}

/// A primitive a copy's values are read as in place: every [`CopyType`] but `Bool`, which is read
/// as its `u8`.
pub trait CopyValue: Copy + private::Sealed {}

mod private {
    pub trait Sealed {}
}

macro_rules! copy_values {
    ($($t:ty),*) => {
        $(impl private::Sealed for $t {}
        impl CopyValue for $t {})*
    };
}
copy_values!(u8, u16, u32, u64, i8, i16, i32, i64, f32, f64);

/// A segment's `bands.bin`, mapped and framed.
///
/// [`Bands::open`] checks what the header and the length can answer, so every slice below is in
/// bounds. It does not check that an entry's row is in the segment, or is the row its identity
/// came from: a reader that indexes a column by an entry's row checks the row against the
/// segment first. Whether the entries are the segment's rows is [`Bands::check_against`]'s
/// question, which reads every entry and is `tessera verify --deep`'s to ask.
#[derive(Debug)]
pub struct Bands {
    map: Mmap,
    first_band: u32,
    row_count: u32,
    starts: Vec<u64>,
    copies: Vec<(String, CopyType)>,
    layout: Layout,
    /// Set once a request has asked for its reads to be advised as random.
    advised: std::sync::Once,
}

impl Bands {
    /// Map and frame the band file in segment directory `dir`, for a segment of `row_count` rows.
    pub fn open(dir: &Path, row_count: u32) -> Result<Self> {
        let path = dir.join(BANDS_FILE);
        let map = map_file(&path)?;
        if map.len() < HEADER_BYTES {
            return Err(malformed(&path, "shorter than the band file's header"));
        }
        if &map[0..4] != MAGIC {
            return Err(malformed(&path, "magic is not TSBD"));
        }
        let read_u16 = |at: usize| u16::from_le_bytes(map[at..at + 2].try_into().unwrap());
        let read_u32 = |at: usize| u32::from_le_bytes(map[at..at + 4].try_into().unwrap());
        let read_u64 = |at: usize| u64::from_le_bytes(map[at..at + 8].try_into().unwrap());
        let version = read_u16(4);
        if version != VERSION {
            return Err(malformed(
                &path,
                format!("version {version}, expected {VERSION}; rebuild the bundle"),
            ));
        }
        let first_band = u32::from(read_u16(6));
        let band_count = read_u32(8) as usize;
        let rows_written = read_u32(12);
        let entries = read_u64(16);
        let copy_count = read_u32(24) as usize;
        if first_band != FIRST_BAND {
            return Err(malformed(
                &path,
                format!("bands begin at {first_band}, expected {FIRST_BAND}"),
            ));
        }
        if rows_written != row_count {
            return Err(malformed(
                &path,
                format!("written for {rows_written} rows, the segment holds {row_count}"),
            ));
        }
        if first_band as usize + band_count > 65 {
            return Err(malformed(&path, format!("{band_count} bands past band 64")));
        }
        let table = HEADER_BYTES + (band_count + 1) * 8 + copy_count * COPY_RECORD_BYTES;
        if map.len() < table {
            return Err(malformed(&path, "shorter than its band and copy tables"));
        }
        let starts: Vec<u64> = (0..=band_count)
            .map(|b| read_u64(HEADER_BYTES + b * 8))
            .collect();
        if starts.first() != Some(&0)
            || starts.last() != Some(&entries)
            || !starts.windows(2).all(|w| w[0] <= w[1])
        {
            return Err(malformed(
                &path,
                "band starts do not ascend from 0 to the entry count",
            ));
        }
        // Nested bands each hold at most the one before.
        if !starts.windows(3).all(|w| w[2] - w[1] <= w[1] - w[0]) {
            return Err(malformed(
                &path,
                "a band holds more entries than a wider one",
            ));
        }
        // The widest band holds at most every row, so with the bands nested the file holds at most
        // 59 entries a row and every size below fits a `usize`. Whether the rows are the segment's
        // is `check_against`'s question.
        if starts.len() > 1 && starts[1] - starts[0] > u64::from(row_count) {
            return Err(malformed(
                &path,
                format!(
                    "band {first_band} holds {} entries for {row_count} rows",
                    starts[1] - starts[0]
                ),
            ));
        }
        let entries = usize::try_from(entries).map_err(|_| malformed(&path, "too many entries"))?;
        let mut copies = Vec::with_capacity(copy_count.min(4096));
        let mut lens = Vec::with_capacity(copy_count);
        let mut at = HEADER_BYTES + (band_count + 1) * 8;
        for _ in 0..copy_count {
            let len = usize::from(read_u16(at));
            let ty = CopyType::of_tag(map[at + 2])
                .ok_or_else(|| malformed(&path, format!("copy type tag {}", map[at + 2])))?;
            if usize::from(map[at + 3]) != ty.width() {
                return Err(malformed(&path, "a copy's width is not its type's"));
            }
            lens.push(len);
            copies.push((String::new(), ty));
            at += COPY_RECORD_BYTES;
        }
        for ((name, _), len) in copies.iter_mut().zip(&lens) {
            let bytes = map
                .get(at..at + len)
                .ok_or_else(|| malformed(&path, "a copy's name runs past the file"))?;
            *name = std::str::from_utf8(bytes)
                .map_err(|_| malformed(&path, "a copy's name is not UTF-8"))?
                .to_string();
            at += len;
        }
        let widths: Vec<usize> = copies.iter().map(|(_, ty)| ty.width()).collect();
        let layout = Layout::of(band_count, entries, lens.iter().sum(), &widths);
        if map.len() != layout.len {
            return Err(malformed(
                &path,
                format!(
                    "{} bytes, but the header describes {}",
                    map.len(),
                    layout.len
                ),
            ));
        }
        Ok(Bands {
            map,
            first_band,
            row_count,
            starts,
            copies,
            layout,
            advised: std::sync::Once::new(),
        })
    }

    /// Advise the kernel that this file is read at scattered pages, once for the mapping: a
    /// request reads a tile's run of a band and a few entries' copies, and read-ahead around each
    /// fault would bring in pages of other tiles. A reader asks for the pages it is about to read
    /// with [`Bands::will_need`] instead. The advice is the mapping's alone; the columns the shipped
    /// sweep reads in order keep the kernel's default.
    pub fn advise_random(&self) {
        self.advised.call_once(|| {
            // Advice only: a failure leaves the default read-ahead, which reads more but the same.
            let _ = self.map.advise(memmap2::Advice::Random);
        });
    }

    /// Ask for the pages holding `range` of `slice`, a section of this file, to be read now and in
    /// the background, so the reads that follow find them in the page cache.
    pub fn will_need<T>(&self, slice: &[T], range: std::ops::Range<usize>) {
        if range.is_empty() {
            return;
        }
        let size = std::mem::size_of::<T>();
        let at = slice.as_ptr() as usize + range.start * size - self.map.as_ptr() as usize;
        let len = range.len() * size;
        debug_assert!(at + len <= self.map.len(), "a range outside the band file");
        // Advice only, as above.
        let _ = self.map.advise_range(memmap2::Advice::WillNeed, at, len);
    }

    /// The segment's row count the file was written for.
    pub fn row_count(&self) -> u32 {
        self.row_count
    }

    /// Every entry, across every band.
    pub fn entries(&self) -> usize {
        self.layout.entries
    }

    /// The bands held: `first_band..first_band + band_count`.
    pub fn bands(&self) -> std::ops::Range<u32> {
        self.first_band..self.first_band + self.layout.band_count as u32
    }

    /// The entries of band `j`, empty for a band the segment has no row in.
    pub fn band(&self, j: u32) -> std::ops::Range<usize> {
        if !self.bands().contains(&j) {
            return 0..0;
        }
        let b = (j - self.first_band) as usize;
        self.starts[b] as usize..self.starts[b + 1] as usize
    }

    fn typed<T>(&self, at: usize) -> &[T] {
        // SAFETY: `open` checked the file is exactly the layout's length, so the section is in
        // bounds; it begins on a page boundary of a page-aligned mapping, so it is aligned for any
        // primitive.
        unsafe { std::slice::from_raw_parts(self.map.as_ptr().add(at) as *const T, self.entries()) }
    }

    /// Each entry's `tessera_id`.
    pub fn ids(&self) -> &[u64] {
        self.typed(self.layout.ids)
    }

    /// Each entry's row in the segment.
    pub fn rows(&self) -> &[u32] {
        self.typed(self.layout.rows)
    }

    /// Each entry's Morton code.
    pub fn codes(&self) -> &[u32] {
        self.typed(self.layout.codes)
    }

    /// Each entry's residual, the low half of its position.
    pub fn residuals(&self) -> &[u32] {
        self.typed(self.layout.residuals)
    }

    /// The names of the copied columns, in the order `columns.arrow` holds them.
    pub fn copy_names(&self) -> impl Iterator<Item = &str> {
        self.copies.iter().map(|(name, _)| name.as_str())
    }

    /// Where one copied column's values lie in `bands.bin`, as a byte range of the file.
    pub fn copy_range(&self, name: &str) -> Option<std::ops::Range<usize>> {
        let k = self.copies.iter().position(|(held, _)| held == name)?;
        let at = self.layout.copies[k];
        Some(at..at + self.entries() * self.copies[k].1.width())
    }

    /// One copied column's values in entry order, read in place as `T`, or `None` where the
    /// segment has no copy of that name stored as `ty`, or `T` is not `ty`'s width.
    pub fn copy_values<T: CopyValue>(&self, name: &str, ty: CopyType) -> Option<&[T]> {
        let k = self.copies.iter().position(|(held, _)| held == name)?;
        if self.copies[k].1 != ty || ty.width() != std::mem::size_of::<T>() {
            return None;
        }
        // SAFETY: `open` checked the file is exactly the layout's length, so the copy's
        // `entries * width` bytes are in bounds; the section begins on a page boundary, so it is
        // aligned for `T`, and every bit pattern is a valid `T`.
        Some(unsafe {
            std::slice::from_raw_parts(
                self.map.as_ptr().add(self.layout.copies[k]) as *const T,
                self.entries(),
            )
        })
    }

    /// One copied column, or `None` where the segment has no column of that name.
    pub fn copy(&self, name: &str) -> Option<BandCopy<'_>> {
        let k = self.copies.iter().position(|(held, _)| held == name)?;
        let ty = self.copies[k].1;
        let at = self.layout.copies[k];
        Some(BandCopy {
            name: &self.copies[k].0,
            ty,
            bytes: &self.map[at..at + self.entries() * ty.width()],
        })
    }

    /// Check every entry against the segment it was written for: each band holds exactly the rows
    /// whose identity has that many leading zeros, in row order, with the row's own identity, code
    /// and residual, and each copy holds the row's stored value. What `tessera verify --deep`
    /// asks; a request trusts the file.
    pub fn check_against(
        &self,
        morton: &[u32],
        columns: &ColumnsRef,
    ) -> std::result::Result<(), String> {
        let ids = columns.tessera_id();
        let residuals = columns.residual();
        if ids.len() != self.row_count as usize || morton.len() != ids.len() {
            return Err(format!(
                "the bands were written for {} rows and the segment holds {}",
                self.row_count,
                ids.len()
            ));
        }
        let names: Vec<&str> = columns
            .scalar_names()
            .filter(|name| !matches!(columns.scalar(name), Some(ScalarSlice::Utf8(_))))
            .collect();
        let held: Vec<&str> = self.copy_names().collect();
        if names != held {
            return Err(format!(
                "the bands copy {held:?} and the segment's render columns are {names:?}"
            ));
        }
        let deepest = ids
            .iter()
            .map(|&id| band_of(id))
            .filter(|&z| z >= FIRST_BAND)
            .max();
        let expected_bands = deepest.map_or(FIRST_BAND..FIRST_BAND, |d| FIRST_BAND..d + 1);
        if self.bands() != expected_bands {
            return Err(format!(
                "the file holds bands {:?} and the segment's identities reach {:?}",
                self.bands(),
                expected_bands
            ));
        }
        let (e_ids, e_rows, e_codes, e_residuals) =
            (self.ids(), self.rows(), self.codes(), self.residuals());
        // One pass over the rows, with a cursor in each band: a row with `z` leading zeros is the
        // next entry of every band up to `z`.
        let bands = self.bands();
        let mut cursor: Vec<usize> = bands.clone().map(|j| self.band(j).start).collect();
        for (row, &id) in ids.iter().enumerate() {
            let zeros = band_of(id);
            for j in bands.start..bands.end.min(zeros + 1) {
                let at = &mut cursor[(j - bands.start) as usize];
                let e = *at;
                if e >= self.band(j).end {
                    return Err(format!("band {j} ends before row {row}"));
                }
                if e_rows[e] as usize != row
                    || e_ids[e] != id
                    || e_codes[e] != morton[row]
                    || e_residuals[e] != residuals[row]
                {
                    return Err(format!(
                        "band {j}, entry {e}: holds row {} where row {row} is expected, or \
                         disagrees with that row's identity, code or residual",
                        e_rows[e]
                    ));
                }
                *at += 1;
            }
        }
        for (j, &e) in bands.clone().zip(&cursor) {
            if e != self.band(j).end {
                return Err(format!(
                    "band {j} holds {} entries past the last row",
                    self.band(j).end - e
                ));
            }
        }
        for name in names {
            let copy = self.copy(name).expect("the names agree");
            let column = columns.scalar(name).expect("listed");
            for (e, &row) in e_rows.iter().enumerate() {
                let stored = column.value_at(row as usize);
                let copied = copy.value_at(e);
                let same = match (&stored, &copied) {
                    (Some(a), b) => scalar_bits_equal(a, b),
                    (None, _) => false,
                };
                if !same {
                    return Err(format!(
                        "copy '{name}', entry {e}: holds {copied:?} and row {row} holds {stored:?}"
                    ));
                }
            }
        }
        Ok(())
    }
}

/// Whether two scalars are the same stored bits; a float compares by its bits, so a NaN copy of a
/// NaN agrees.
fn scalar_bits_equal(
    a: &tessera_types::scalar::ScalarValue,
    b: &tessera_types::scalar::ScalarValue,
) -> bool {
    use tessera_types::scalar::ScalarValue as V;
    match (a, b) {
        (V::F32(x), V::F32(y)) => x.to_bits() == y.to_bits(),
        (V::F64(x), V::F64(y)) => x.to_bits() == y.to_bits(),
        _ => a == b,
    }
}

/// A segment's `cell-codes.u32`, mapped.
#[derive(Debug)]
pub struct CellCodes {
    map: Mmap,
}

impl CellCodes {
    /// Map the cell codes in segment directory `dir`, for a segment of `cells` occupied cells.
    pub fn open(dir: &Path, cells: usize) -> Result<Self> {
        let path = dir.join(CELL_CODES_FILE);
        let map = map_file(&path)?;
        if map.len() != cells * 4 {
            return Err(malformed(
                &path,
                format!("{} bytes for {cells} cells", map.len()),
            ));
        }
        Ok(CellCodes { map })
    }

    /// Each occupied cell's Morton code, in the order `cuts.u32` lists the cells.
    pub fn codes(&self) -> &[u32] {
        // SAFETY: the length is the cell count times four, checked at open, and a mapping is
        // page-aligned.
        unsafe { std::slice::from_raw_parts(self.map.as_ptr() as *const u32, self.map.len() / 4) }
    }
}

// ---------------------------------------------------------------------------------------------
// A level's labels, in band-entry order
// ---------------------------------------------------------------------------------------------

const LABELS_MAGIC: &[u8; 4] = b"TSBL";
const LABELS_VERSION: u16 = 1;
const LABELS_HEADER_BYTES: usize = 24;

/// Write `path`: the label `labels` gives each entry of `bands`, in entry order, at the column's
/// own width and with its own hole. A level's label column and this copy are written together and
/// at no other time, so the copy holds what the column holds at its rows.
pub fn write_band_labels(
    path: &Path,
    bands: &Bands,
    labels: &crate::membership::LabelColumnPack,
) -> Result<()> {
    if labels.rows() != bands.row_count() {
        return Err(StoreError::MalformedBundle {
            detail: format!(
                "band labels: the label column covers {} rows and the segment holds {}",
                labels.rows(),
                bands.row_count()
            ),
        });
    }
    let io = |source| StoreError::Io {
        path: path.to_path_buf(),
        source,
    };
    let width = labels.width();
    let mut out = BufWriter::with_capacity(1 << 20, File::create(path).map_err(io)?);
    let mut header = [0u8; LABELS_HEADER_BYTES];
    header[0..4].copy_from_slice(LABELS_MAGIC);
    header[4..6].copy_from_slice(&LABELS_VERSION.to_le_bytes());
    header[6] = width;
    header[8..16].copy_from_slice(&(bands.entries() as u64).to_le_bytes());
    header[16..20].copy_from_slice(&bands.row_count().to_le_bytes());
    header[20..24].copy_from_slice(&labels.ordinals().to_le_bytes());
    out.write_all(&header).map_err(io)?;
    // Every entry's row is read in order, and a mapping a request has advised as random has no
    // read-ahead of its own.
    bands.will_need(bands.rows(), 0..bands.entries());
    for &row in bands.rows() {
        let label = labels.label(row as usize);
        let bytes = label.to_le_bytes();
        out.write_all(&bytes[..usize::from(width)]).map_err(io)?;
    }
    out.flush().map_err(io)
}

/// A level's labels in one segment's band-entry order, mapped and framed.
#[derive(Debug)]
pub struct BandLabels {
    map: Mmap,
    width: u8,
    entries: usize,
    ordinals: u32,
}

impl BandLabels {
    /// Map `path`, written for `bands`.
    pub fn open(path: &Path, bands: &Bands) -> Result<Self> {
        let map = map_file(path)?;
        if map.len() < LABELS_HEADER_BYTES || &map[0..4] != LABELS_MAGIC {
            return Err(malformed(path, "not a band label file"));
        }
        let version = u16::from_le_bytes([map[4], map[5]]);
        if version != LABELS_VERSION {
            return Err(malformed(
                path,
                format!("version {version}, expected {LABELS_VERSION}; rebuild the bundle"),
            ));
        }
        let width = map[6];
        if !matches!(width, 1 | 2 | 4) {
            return Err(malformed(path, format!("label width {width}")));
        }
        let entries = u64::from_le_bytes(map[8..16].try_into().unwrap()) as usize;
        let rows = u32::from_le_bytes(map[16..20].try_into().unwrap());
        let ordinals = u32::from_le_bytes(map[20..24].try_into().unwrap());
        if entries != bands.entries() || rows != bands.row_count() {
            return Err(malformed(
                path,
                format!(
                    "written for {entries} entries over {rows} rows, the segment's bands hold {} \
                     over {}",
                    bands.entries(),
                    bands.row_count()
                ),
            ));
        }
        if map.len() != LABELS_HEADER_BYTES + entries * usize::from(width) {
            return Err(malformed(path, "the length is not the header's"));
        }
        Ok(BandLabels {
            map,
            width,
            entries,
            ordinals,
        })
    }

    pub fn entries(&self) -> usize {
        self.entries
    }

    pub fn ordinals(&self) -> u32 {
        self.ordinals
    }

    /// Entry `e`'s label, or [`crate::membership::ROW_COLUMN_HOLE`] where its row belongs to no
    /// artifact of the level.
    pub fn label(&self, e: usize) -> u32 {
        let w = usize::from(self.width);
        let at = LABELS_HEADER_BYTES + e * w;
        let mut bytes = [0u8; 4];
        bytes[..w].copy_from_slice(&self.map[at..at + w]);
        let value = u32::from_le_bytes(bytes);
        let hole = if w == 4 {
            u32::MAX
        } else {
            (1u32 << (8 * w)) - 1
        };
        if value == hole {
            crate::membership::ROW_COLUMN_HOLE
        } else {
            value
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::write::write_segment;
    use tessera_spatial::tiler::{ScalarType, ScalarValue, TilerItem};
    use tessera_types::TesseraId;

    /// A segment of `n` rows whose identities spread over many bands: row `i`'s identity has `i % 20`
    /// leading zeros. Rows ascend by Morton code, three rows a cell.
    fn segment(dir: &Path, n: usize) -> Vec<TilerItem> {
        let items: Vec<TilerItem> = (0..n)
            .map(|i| {
                let zeros = (i % 20) as u32;
                let id = (u64::MAX >> zeros) - i as u64;
                TilerItem {
                    tessera_id: TesseraId::new(id),
                    qx: ((i / 3) as u32) << 17,
                    qy: 7,
                    scalars: vec![
                        ScalarValue::U16(i as u16),
                        ScalarValue::Bool(i % 2 == 0),
                        ScalarValue::F64(i as f64 / 4.0),
                    ],
                }
            })
            .collect();
        let mut order: Vec<usize> = (0..n).collect();
        let key = |i: usize| {
            let code = tessera_spatial::split32(items[i].qx, items[i].qy).0.raw();
            (code, items[i].tessera_id.raw())
        };
        order.sort_by_key(|&i| key(i));
        let sorted: Vec<TilerItem> = order.iter().map(|&i| items[i].clone()).collect();
        let codes: Vec<u32> = sorted
            .iter()
            .map(|it| tessera_spatial::split32(it.qx, it.qy).0.raw())
            .collect();
        write_segment(
            dir,
            &sorted,
            &codes,
            &[
                ("count".to_string(), ScalarType::U16),
                ("flag".to_string(), ScalarType::Bool),
                ("weight".to_string(), ScalarType::F64),
            ],
        )
        .unwrap();
        sorted
    }

    #[test]
    fn every_band_holds_its_rows_in_row_order_with_their_values() {
        let dir = tempfile::TempDir::new().unwrap();
        let rows = segment(dir.path(), 400);
        let columns = ColumnsRef::load(&dir.path().join("columns.arrow")).unwrap();
        let morton = crate::read::MortonSlice::load(&dir.path().join("morton.u32")).unwrap();
        let bands = Bands::open(dir.path(), 400).unwrap();
        bands.check_against(morton.u32(), &columns).unwrap();

        assert_eq!(bands.bands(), FIRST_BAND..20);
        for j in bands.bands() {
            let expected: Vec<u32> = rows
                .iter()
                .enumerate()
                .filter(|(_, it)| band_of(it.tessera_id.raw()) >= j)
                .map(|(row, _)| row as u32)
                .collect();
            assert_eq!(
                &bands.rows()[bands.band(j)],
                expected.as_slice(),
                "band {j}"
            );
        }
        let count = bands.copy("count").unwrap();
        let weight = bands.copy("weight").unwrap();
        let flag = bands.copy("flag").unwrap();
        for (e, &row) in bands.rows().iter().enumerate() {
            assert_eq!(
                Some(count.value_at(e)),
                columns.scalar("count").unwrap().value_at(row as usize)
            );
            assert_eq!(
                Some(weight.value_at(e)),
                columns.scalar("weight").unwrap().value_at(row as usize)
            );
            assert_eq!(
                Some(flag.value_at(e)),
                columns.scalar("flag").unwrap().value_at(row as usize)
            );
        }
        assert!(bands.copy("absent").is_none());
    }

    #[test]
    fn the_cell_codes_are_each_cells_code() {
        let dir = tempfile::TempDir::new().unwrap();
        segment(dir.path(), 100);
        let morton = crate::read::MortonSlice::load(&dir.path().join("morton.u32")).unwrap();
        let cuts = crate::read::CutIndex::load(&dir.path().join(crate::read::CutIndex::FILE), 100)
            .unwrap();
        let cells = CellCodes::open(dir.path(), cuts.len()).unwrap();
        let expected: Vec<u32> = cuts
            .starts()
            .iter()
            .map(|&s| morton.u32()[s as usize])
            .collect();
        assert_eq!(cells.codes(), expected.as_slice());
        assert!(CellCodes::open(dir.path(), cuts.len() + 1).is_err());
    }

    #[test]
    fn a_segment_with_no_row_in_the_first_band_has_an_empty_file_that_opens() {
        let dir = tempfile::TempDir::new().unwrap();
        let items = vec![TilerItem {
            tessera_id: TesseraId::new(u64::MAX),
            qx: 1,
            qy: 1,
            scalars: vec![],
        }];
        let code = tessera_spatial::split32(1, 1).0.raw();
        write_segment(dir.path(), &items, &[code], &[]).unwrap();
        let bands = Bands::open(dir.path(), 1).unwrap();
        assert_eq!(bands.entries(), 0);
        assert!(bands.bands().is_empty());
        assert_eq!(bands.band(FIRST_BAND), 0..0);
        assert!(!dir.path().join(format!("{BANDS_FILE}.spool")).exists());
    }

    #[test]
    fn a_file_for_another_segment_or_a_damaged_one_is_refused() {
        let dir = tempfile::TempDir::new().unwrap();
        segment(dir.path(), 200);
        assert!(Bands::open(dir.path(), 199).is_err());
        let path = dir.path().join(BANDS_FILE);
        let mut bytes = std::fs::read(&path).unwrap();
        bytes.truncate(bytes.len() - 1);
        std::fs::write(&path, &bytes).unwrap();
        assert!(Bands::open(dir.path(), 200).is_err());
    }

    /// A header claiming more entries in the widest band than the segment has rows, or a count past
    /// what a file could hold, is refused rather than read.
    #[test]
    fn a_header_claiming_more_entries_than_rows_is_refused() {
        let dir = tempfile::TempDir::new().unwrap();
        segment(dir.path(), 200);
        let path = dir.path().join(BANDS_FILE);
        let good = std::fs::read(&path).unwrap();
        for (starts_1, entries) in [(201u64, 201u64), (u64::MAX, u64::MAX)] {
            let mut bytes = good.clone();
            bytes[8..12].copy_from_slice(&1u32.to_le_bytes());
            bytes[16..24].copy_from_slice(&entries.to_le_bytes());
            bytes[HEADER_BYTES..HEADER_BYTES + 8].copy_from_slice(&0u64.to_le_bytes());
            bytes[HEADER_BYTES + 8..HEADER_BYTES + 16].copy_from_slice(&starts_1.to_le_bytes());
            std::fs::write(&path, &bytes).unwrap();
            assert!(matches!(
                Bands::open(dir.path(), 200),
                Err(StoreError::MalformedBundle { .. })
            ));
        }
    }

    #[test]
    fn a_changed_entry_fails_the_check() {
        let dir = tempfile::TempDir::new().unwrap();
        segment(dir.path(), 200);
        let path = dir.path().join(BANDS_FILE);
        let bands = Bands::open(dir.path(), 200).unwrap();
        let rows_at = bands.layout.rows;
        drop(bands);
        let mut bytes = std::fs::read(&path).unwrap();
        bytes[rows_at] ^= 1;
        std::fs::write(&path, &bytes).unwrap();
        let columns = ColumnsRef::load(&dir.path().join("columns.arrow")).unwrap();
        let morton = crate::read::MortonSlice::load(&dir.path().join("morton.u32")).unwrap();
        let bands = Bands::open(dir.path(), 200).unwrap();
        assert!(bands.check_against(morton.u32(), &columns).is_err());
    }

    /// A copy that cannot be taken, here because the column labels another segment's rows, is an
    /// error and leaves nothing in the scratch directory: the level is then filed without a copy.
    #[test]
    fn a_copy_that_cannot_be_staged_leaves_nothing_behind() {
        let dir = tempfile::TempDir::new().unwrap();
        segment(dir.path(), 300);
        let bands = Bands::open(dir.path(), 300).unwrap();
        let column = dir.path().join("column.tslb");
        std::fs::write(
            &column,
            crate::membership::pack_label_column(3, &vec![1u32; 299]),
        )
        .unwrap();
        let scratch = dir.path().join("scratch");
        std::fs::create_dir_all(&scratch).unwrap();
        assert!(crate::derived::stage_band_labels(&column, &bands, &scratch).is_err());
        assert_eq!(std::fs::read_dir(&scratch).unwrap().count(), 0);
    }

    #[test]
    fn band_labels_hold_the_columns_label_at_each_entrys_row() {
        let dir = tempfile::TempDir::new().unwrap();
        segment(dir.path(), 300);
        let bands = Bands::open(dir.path(), 300).unwrap();
        let labels: Vec<u32> = (0..300u32)
            .map(|row| if row % 7 == 0 { u32::MAX } else { row % 5 })
            .collect();
        let pack = crate::membership::LabelColumnPack::from_bytes(
            crate::membership::pack_label_column(5, &labels),
        )
        .unwrap();
        let path = dir.path().join("labels.tsbl");
        write_band_labels(&path, &bands, &pack).unwrap();
        let copy = BandLabels::open(&path, &bands).unwrap();
        assert_eq!(copy.entries(), bands.entries());
        for (e, &row) in bands.rows().iter().enumerate() {
            assert_eq!(
                copy.label(e),
                pack.label(row as usize),
                "entry {e}, row {row}"
            );
        }
    }
}
