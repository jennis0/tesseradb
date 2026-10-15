//! Sorted runs of `(key, entity)` entries, and the lookups, writers, merge and verifier over them.
//! One format serves every index that maps a value to entities: a unique field's values (keys from
//! [`unsigned_key`], [`signed_key`] or [`keyword_key`]) and a map from one `u32` to another.
//!
//! # The run file
//!
//! A run is one file holding entries sorted strictly ascending by `(key, entity)`, where a key is
//! an unsigned integer of 4, 8 or 16 bytes ([`Key`]) and an entity is a `u32`. The same key may
//! appear with several entities; what that means is the caller's to decide. Every integer is
//! little-endian.
//!
//! | Offset | Bytes | Content |
//! | --- | --- | --- |
//! | 0 | 4096 | Header page: [`MAGIC`], [`FORMAT_VERSION`] (u32), key width (u32), entry count (u64), page count (u64), smallest key (u128), largest key (u128), key filter offset (u64), key filter blocks (u64), key filter hashes (u32), then a CRC-32 of those 84 bytes; zeros to the end of the page |
//! | 4096 × (1 + p) | 4096 | Entry page `p`, below |
//! | 4096 × (1 + pages) | pages × width + 4 | Page index: the first key of every entry page, then a CRC-32 of those keys |
//! | the next multiple of 64 | blocks × 64 + 4 | Key filter, when the run has one: zeros up to its offset, its blocks, then a CRC-32 of the blocks |
//!
//! A run without a key filter has zero for its offset, blocks and hashes and ends at its page
//! index. A run with one has at least one block and from one to seven hashes; the filter's layout
//! and hash are in the `filter` module's doc, and every key in the run passes it.
//!
//! An entry page holds its entries' keys as the first key and the gaps from each key to the next,
//! all packed at one bit width, the width of the page's widest gap; and its entities as they are.
//!
//! | Bytes | Content |
//! | --- | --- |
//! | 2 | Entry count `n`, at least 1 |
//! | 1 | Gap width `b` in bits, from 0 to 8 × the key width |
//! | key width | The first key |
//! | ⌈(n − 1) × b / 8⌉ | The `n − 1` gaps, each `b` bits, least significant bit first, the first gap in the lowest bits; zero bits after the last |
//! | 4 × n | The entities, in entry order |
//! | to byte 4092 | Zeros |
//! | 4 | CRC-32 of the page's first 4092 bytes |
//!
//! A writer fills a page with entries until the next would not fit at the gap width the page
//! would then need, and an entry never straddles a page, so an entry page is read and checked on
//! its own. Record numbers with a few holes pack at two to five bits a gap, about 4.4 bytes an
//! entry. Keys spread across their whole width, as keyword hashes in a small run are, pack at the
//! full key width: 511, 340 and 204 entries a page for 4-, 8- and 16-byte keys. A key held by
//! several entities is a gap of zero.
//!
//! Finding a key decodes its page's gaps from the first entry up to the key.
//!
//! Opening a run reads the header, the page index and the key filter, and nothing else. A lookup
//! reads a page only for a key in the run's key range that its filter, where it has one, passes.
//! An entry page's checksum, its order, its agreement with the page index and its order against
//! any neighbouring page already read are checked the first time the page is read, and the run
//! remembers which pages have passed. A page, header, page index or key filter that fails is a
//! [`StoreError::InvalidKeyIndex`] naming the file and the part, never an empty answer.
//! [`verify_run`] checks every page at once, and that the filter passes every key.
//!
//! # Writing and reading
//!
//! [`KeyRunWriter`] writes sorted entries, starting a new run file at the first key change past a
//! given entry count, so no run has a size limit and the runs one writer emits have disjoint key
//! ranges. A writer made [`KeyRunWriter::with_key_filter`] gives every run a key filter, filled
//! from the run's pages once they are written. The header page is written last, so a file cut short while it was written fails its header check. No writer here
//! fsyncs: a caller publishing runs passes their paths to [`crate::fsync_written`] before the
//! manifest that names them.
//!
//! [`KeySpill`] takes entries in any order under a memory budget, sorts them through a scratch
//! directory and writes them the same way, reporting every key held by more than one entity.
//! [`merge_runs`] merges runs into new ones through a writer, dropping the entries a caller's test
//! picks out.
//!
//! [`KeyRun`] answers for one run; [`KeyIndexView`] answers for a newest-first list of runs any
//! key may be in plus a list of runs with disjoint key ranges, of which a lookup reads only the one
//! whose range holds the key.

mod filter;
mod key;
mod merge;
mod page;
mod run;
mod spill;
mod view;
mod write;

#[cfg(test)]
mod tests;

use std::fmt;

pub use key::{keyword_key, signed_key, signed_value, unsigned_key, Key};
pub use merge::{for_each_merged, merge_runs};
pub use run::{run_key_width, verify_run, Entries, KeyRun, RunCheck};
pub use spill::{DuplicateKey, KeySpill};
pub use view::{Found, KeyIndexView, RunRef};
pub use write::{KeyRunWriter, WrittenRun};

/// The first eight bytes of every run file.
pub const MAGIC: [u8; 8] = *b"MSKEYRUN";

/// The run file's own format number. A file carrying another is refused at open.
pub const FORMAT_VERSION: u32 = 4;

/// The size of the header page, of every entry page, and of the unit a page checksum covers.
pub const PAGE_SIZE: usize = 4096;

/// Bytes of the header the header checksum covers.
const HEADER_LEN: usize = 84;

/// Bytes of an entry page before its checksum.
const PAGE_BODY: usize = PAGE_SIZE - 4;

/// Which part of a run file a check failed in.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RunPart {
    Header,
    Page(u64),
    PageIndex,
    Filter,
}

impl fmt::Display for RunPart {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            RunPart::Header => write!(f, "header"),
            RunPart::Page(p) => write!(f, "page {p}"),
            RunPart::PageIndex => write!(f, "page index"),
            RunPart::Filter => write!(f, "key filter"),
        }
    }
}

/// The header's fields, as the writer records them and the reader checks them.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct Header {
    key_width: u32,
    entries: u64,
    pages: u64,
    min_key: u128,
    max_key: u128,
    filter: FilterShape,
}

/// Where a run's key filter is and how it hashes; all zero for a run without one.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
struct FilterShape {
    offset: u64,
    blocks: u64,
    hashes: u32,
}

impl Header {
    /// The header page: fields, checksum, zeros.
    fn encode(&self) -> Vec<u8> {
        let mut page = vec![0u8; PAGE_SIZE];
        page[0..8].copy_from_slice(&MAGIC);
        page[8..12].copy_from_slice(&FORMAT_VERSION.to_le_bytes());
        page[12..16].copy_from_slice(&self.key_width.to_le_bytes());
        page[16..24].copy_from_slice(&self.entries.to_le_bytes());
        page[24..32].copy_from_slice(&self.pages.to_le_bytes());
        page[32..48].copy_from_slice(&self.min_key.to_le_bytes());
        page[48..64].copy_from_slice(&self.max_key.to_le_bytes());
        page[64..72].copy_from_slice(&self.filter.offset.to_le_bytes());
        page[72..80].copy_from_slice(&self.filter.blocks.to_le_bytes());
        page[80..84].copy_from_slice(&self.filter.hashes.to_le_bytes());
        let crc = crc32fast::hash(&page[..HEADER_LEN]);
        page[HEADER_LEN..HEADER_LEN + 4].copy_from_slice(&crc.to_le_bytes());
        page
    }

    /// The fields of a header page, checked against its checksum, magic and version. Checks that
    /// need the file's length or the caller's key width are the reader's.
    fn decode(bytes: &[u8]) -> std::result::Result<Header, String> {
        if bytes.len() < PAGE_SIZE {
            return Err(format!(
                "the file is {} bytes, shorter than its {PAGE_SIZE}-byte header page",
                bytes.len()
            ));
        }
        if bytes[0..8] != MAGIC {
            return Err("the file does not start with the key run magic".to_string());
        }
        let stored = u32::from_le_bytes(bytes[HEADER_LEN..HEADER_LEN + 4].try_into().unwrap());
        if crc32fast::hash(&bytes[..HEADER_LEN]) != stored {
            return Err("the header checksum does not match its bytes".to_string());
        }
        let version = u32::from_le_bytes(bytes[8..12].try_into().unwrap());
        if version != FORMAT_VERSION {
            return Err(format!(
                "the run is format {version} and this reader reads format {FORMAT_VERSION}; \
                 rebuild the index"
            ));
        }
        let u64_at = |at: usize| u64::from_le_bytes(bytes[at..at + 8].try_into().unwrap());
        let u128_at = |at: usize| u128::from_le_bytes(bytes[at..at + 16].try_into().unwrap());
        Ok(Header {
            key_width: u32::from_le_bytes(bytes[12..16].try_into().unwrap()),
            entries: u64_at(16),
            pages: u64_at(24),
            min_key: u128_at(32),
            max_key: u128_at(48),
            filter: FilterShape {
                offset: u64_at(64),
                blocks: u64_at(72),
                hashes: u32::from_le_bytes(bytes[80..84].try_into().unwrap()),
            },
        })
    }
}

/// Where the page index of a run of `pages` entry pages with `width`-byte keys ends.
fn index_end(pages: u64, width: usize) -> Option<u64> {
    let entry_bytes = pages.checked_add(1)?.checked_mul(PAGE_SIZE as u64)?;
    let index_bytes = pages.checked_mul(width as u64)?.checked_add(4)?;
    entry_bytes.checked_add(index_bytes)
}

/// Where the key filter of such a run starts: the first block boundary at or past the index's end.
fn filter_offset(pages: u64, width: usize) -> Option<u64> {
    index_end(pages, width)?.checked_next_multiple_of(filter::BLOCK as u64)
}

/// The file length a run with this header has, for `width`-byte keys.
fn file_len(header: &Header, width: usize) -> Option<u64> {
    if header.filter.blocks == 0 {
        return index_end(header.pages, width);
    }
    header
        .filter
        .blocks
        .checked_mul(filter::BLOCK as u64)?
        .checked_add(4)?
        .checked_add(header.filter.offset)
}
