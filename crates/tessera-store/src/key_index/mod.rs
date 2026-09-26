//! Sorted runs of fixed-width `(key, entity)` entries, and the lookups, writers, merge and
//! verifier over them. One format serves every index that maps a value to entities: a unique
//! field's values (keys from [`unsigned_key`], [`signed_key`] or [`keyword_key`]) and a map from
//! one `u32` to another.
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
//! | 0 | 4096 | Header page: [`MAGIC`], [`FORMAT_VERSION`] (u32), key width (u32), entry count (u64), page count (u64), smallest key (u128), largest key (u128), then a CRC-32 of those 64 bytes; zeros to the end of the page |
//! | 4096 × (1 + p) | 4096 | Entry page `p`: as many `key ‖ entity` entries as fit before the page's last four bytes, zeros after the last entry, and a CRC-32 of the page's first 4092 bytes in its last four |
//! | 4096 × (1 + pages) | pages × width + 4 | Page index: the first key of every entry page, then a CRC-32 of those keys |
//!
//! An entry never straddles a page, so an entry page is read and checked on its own. The widths
//! give 511, 341 and 204 entries a page.
//!
//! Opening a run reads the header and the page index and nothing else. An entry page's checksum,
//! its order and its agreement with the page index are checked the first time the page is read,
//! and the run remembers which pages have passed. A page, header or page index that fails is a
//! [`StoreError::InvalidKeyIndex`] naming the file and the part, never an empty answer.
//! [`verify_run`] checks every page at once.
//!
//! # Writing and reading
//!
//! [`KeyRunWriter`] writes sorted entries, starting a new run file at the first key change past a
//! given entry count, so no run has a size limit and the runs one writer emits have disjoint key
//! ranges. [`KeySpill`] takes entries in any order under a memory budget, sorts them through a
//! scratch directory and writes them the same way, reporting every key held by more than one
//! entity. [`merge_runs`] merges runs into new ones, dropping the entries of a given set of
//! entities.
//!
//! [`KeyRun`] answers for one run; [`KeyIndexView`] answers for a newest-first list of runs any
//! key may be in plus a list of runs with disjoint key ranges, of which a lookup reads only the one
//! whose range holds the key.

mod key;
mod merge;
mod run;
mod spill;
mod view;
mod write;

#[cfg(test)]
mod tests;

use std::fmt;

pub use key::{keyword_key, signed_key, signed_value, unsigned_key, Key};
pub use merge::merge_runs;
pub use run::{verify_run, Entries, KeyRun, RunCheck};
pub use spill::KeySpill;
pub use view::{Found, KeyIndexView, RunRef};
pub use write::{KeyRunWriter, WrittenRun};

/// The first eight bytes of every run file.
pub const MAGIC: [u8; 8] = *b"TSKEYRUN";

/// The run file's own format number. A file carrying another is refused at open.
pub const FORMAT_VERSION: u32 = 1;

/// The size of the header page, of every entry page, and of the unit a page checksum covers.
pub const PAGE_SIZE: usize = 4096;

/// Bytes of the header the header checksum covers.
const HEADER_LEN: usize = 64;

/// Bytes of an entry page before its checksum.
const PAGE_BODY: usize = PAGE_SIZE - 4;

/// Entries in one page for keys `width` bytes wide.
pub const fn entries_per_page(width: usize) -> usize {
    PAGE_BODY / (width + 4)
}

/// Which part of a run file a check failed in.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RunPart {
    Header,
    Page(u64),
    PageIndex,
}

impl fmt::Display for RunPart {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            RunPart::Header => write!(f, "header"),
            RunPart::Page(p) => write!(f, "page {p}"),
            RunPart::PageIndex => write!(f, "page index"),
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
        let stored = u32::from_le_bytes(bytes[HEADER_LEN..HEADER_LEN + 4].try_into().unwrap());
        if crc32fast::hash(&bytes[..HEADER_LEN]) != stored {
            return Err("the header checksum does not match its bytes".to_string());
        }
        if bytes[0..8] != MAGIC {
            return Err("the file does not start with the key run magic".to_string());
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
        })
    }
}

/// The file length a run of `pages` entry pages with `width`-byte keys has.
fn file_len(pages: u64, width: usize) -> Option<u64> {
    let entry_bytes = pages.checked_add(1)?.checked_mul(PAGE_SIZE as u64)?;
    let index_bytes = pages.checked_mul(width as u64)?.checked_add(4)?;
    entry_bytes.checked_add(index_bytes)
}
