//! `Permutation`: the **only** legal EntityId→RowId path in the codebase (invariant I4).
//! Backed by `permutation.bin` (R4), the **two-level paged** entity→row map: a directory over
//! pages of 2¹⁶ consecutive entity ids, an absent page meaning every entity in it has no row in
//! this segment.
//!
//! ## The file
//!
//! ```text
//!   0   "TSPM"                        magic
//!   4   u16 version = 2
//!   6   u16 page_shift = 16           recorded, not assumed — any other value is refused
//!   8   u64 bound                     the entity ids this file covers, [0, bound)
//!  16   u32 page_count                = ceil(bound / 2^16)
//!  20   u32 present_count             how many of them carry slots
//!  24   u32 × page_count              the directory: each page's slot in the payload, or
//!                                     0xFFFF_FFFF for an absent page
//!       zero padding                  to the next 4096-byte boundary
//!  ...  u32 × 2^16 × present_count    the pages, in slot order, which is ascending page order
//! ```
//!
//! A slot is a row id, sentinel `0xFFFF_FFFF` for an entity with no row here — so a *present*
//! page may still hold holes, and an *absent* page is exactly one that holds nothing but holes.
//! **A dense view is the degenerate case with every page present** (`views.md` §8): the flat
//! array of earlier revisions plus an identity directory. A sparse one — a group's quarterly view
//! holding 8k of 21k entities — stores the pages it occupies and nothing else, which is the whole
//! reason the level exists: at 10⁹ entities a flat array is 4 GB per view, sentinel-dominated,
//! multiplied by the views of the group.
//!
//! Nothing is compressed and nothing is decoded (contracts §2.6 — mmap and slice). The payload
//! starts on a 4 KiB boundary and a page is 256 KiB, so every page is page-aligned in the mapping.
//!
//! **Canonical, so the bytes are a function of the mapping.** Slots ascend with page index and
//! number `0..present_count` exactly; the padding is zero; the tail of the last page above `bound`
//! is sentinel. A file departing from any of these is refused at load rather than read generously,
//! which is what lets the two producers — the build's planned writer and the fold's scatter
//! ([`crate::write::PermutationWriter`]) — be held to byte-for-byte agreement.
//!
//! **The flat array is gone, and the version number refuses it by construction.** Version 1 was
//! `bound` slots with no directory and no page count; this is version 2, so a file from the older
//! producer fails [`Permutation::load`] with an unsupported-version error rather than having its
//! first slots read as a directory. The artifacts are recreated rather than carried
//! ([decision 0048](../../../docs/decisions/0048-no-deployments-exist-so-delete-rather-than-support.md)),
//! and `bundle_format` does not move (owner direction) — the loud refusal a bump would have
//! supplied is delivered by the version field, which is the field that actually changed.
//!
//! ## Row space is that file plus an ordered extent list
//!
//! A build writes one segment per (partition, view) and one `permutation.bin` covering it. A
//! flush appends a segment beside it, and [`RowSpace`] is what makes the pair addressable as one
//! row space: the base file below the build bound, an ordered list of [`SegmentExtent`]s above it.
//!
//! **The dispatch lives here rather than in the engine, and that is I4 rather than tidiness.**
//! The claim this module makes about itself — that it is the only legal EntityId→RowId path — is
//! falsified by an engine that learns to select an extent and index a segment. Every caller still
//! sees `row_of` and `project`; which segment answered is this module's business, and so is
//! whether the answer came from a page or from an absent one.
//!
//! Two bounds hold by construction and are checked at the one place an extent enters
//! ([`RowSpace::with_extent`]): total rows per view stay under 2³², and the extent list is
//! bounded by the live segment count, which the merge policy bounds.

use std::fs::File;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use memmap2::Mmap;

use tessera_roaring::{Sink, BLOCK, WORDS};
use tessera_types::{EntityId, RowId, ROW_ABSENT};

use crate::error::{Result, StoreError};

/// A row-indexed "already claimed" set, one bit a row.
///
/// The two bijectivity checks in this module sweep a row space that reaches 10⁹. A byte a row
/// is 1 GB of transient memory there, paid by every view at bundle open and twice again by
/// `tessera verify`; a bit a row is 125 MB. The check is unchanged: a row whose bit is already
/// set is claimed by a second entity.
struct RowsSeen(Vec<u64>);

impl RowsSeen {
    fn new(rows: usize) -> RowsSeen {
        RowsSeen(vec![0u64; rows.div_ceil(64)])
    }

    /// Claim `row`, returning whether it was already claimed.
    fn claim(&mut self, row: usize) -> bool {
        let bit = 1u64 << (row & 63);
        let word = &mut self.0[row >> 6];
        let already = *word & bit != 0;
        *word |= bit;
        already
    }
}

const PERMUTATION_MAGIC: &[u8; 4] = b"TSPM";
const PERMUTATION_VERSION: u16 = 2;

/// A page covers `2^PAGE_SHIFT` consecutive entity ids.
///
/// 16 is the owner's ruling (2026-08-30) and is also the width that makes the two levels cost
/// what they should: a page is 256 KiB, so a directory entry costs 4 bytes per 256 KiB of covered
/// entity space (a 65 536-entry directory at the `u32` entity ceiling, 256 KiB in total), while a
/// view sparse at any coarser granularity than a page still pays only for the pages it lands in.
/// It is recorded in the header rather than assumed, so a file written at another width is a
/// refusal and not a misread.
pub const PAGE_SHIFT: u32 = 16;

/// Entity slots in one page.
pub const PAGE_ENTRIES: usize = 1 << PAGE_SHIFT;

/// Bytes in one page — `PAGE_ENTRIES` little-endian `u32` row ids.
pub const PAGE_BYTES: usize = PAGE_ENTRIES * 4;

/// The directory entry for a page that carries no slots: every entity in it is row-absent.
///
/// The same bit pattern as [`ROW_ABSENT`], and deliberately so — both mean *no row*, one page at a
/// time and one entity at a time.
pub const PAGE_ABSENT: u32 = 0xFFFF_FFFF;

/// magic, version, page_shift, bound, page_count, present_count.
pub(crate) const HEADER_LEN: usize = 4 + 2 + 2 + 8 + 4 + 4;

/// The payload's alignment. A page is 256 KiB, so aligning the first one aligns them all; 4 KiB is
/// the mapping granularity every platform this targets shares.
pub(crate) const PAGE_ALIGN: usize = 4096;

/// Where the pages begin, given the directory's length: the header and directory rounded up to
/// [`PAGE_ALIGN`]. The one arithmetic both the reader and the writer depend on, so it has one
/// definition site.
pub(crate) fn payload_start(page_count: usize) -> usize {
    (HEADER_LEN + page_count * 4).next_multiple_of(PAGE_ALIGN)
}

/// How many pages cover `[0, bound)`.
pub(crate) fn pages_for(bound: u64) -> u64 {
    bound.div_ceil(PAGE_ENTRIES as u64)
}

/// Rows per bucket in [`Permutation::project`], as a power of two — the one free parameter in that
/// pass, and the two constraints that fix it pull in opposite directions.
///
/// A bucket owns a contiguous range of row space. Too *wide* and the bit array it stamps falls out
/// of L2, making every row's stamp a last-level miss; too *narrow* and there are so many live write
/// cursors in the first pass that the append side misses instead. 2²² rows is a 512 KB bit array,
/// which is L2-resident on the machines this targets, and leaves 239 buckets at 10⁹ — whose cursors
/// and write tails together stay inside L1.
///
/// It is also a whole number of Roaring containers (64 of them), which is not a coincidence to be
/// preserved by luck: the emit step hands each container's words straight to [`Sink`], so a bucket
/// boundary that fell inside a container would split a payload across two of them.
const BUCKET_SHIFT: u32 = 22;

/// `u64` words in one bucket's bit array — 2²² rows, 512 KB.
const STAMP_WORDS: usize = (1usize << BUCKET_SHIFT) / 64;

/// Roaring containers in one bucket. [`BUCKET_SHIFT`] is chosen so this is a whole number.
const CONTAINERS_PER_BUCKET: usize = (1usize << BUCKET_SHIFT) / BLOCK;

/// `u64` words in the mark array, one bit per word of the stamp.
const MARK_WORDS: usize = STAMP_WORDS / 64;

/// Mark words covering one container's 1,024 stamp words.
const MARKS_PER_CONTAINER: usize = WORDS / 64;

/// Rows in a bucket below which the emit reads only the stamp words that hold something.
///
/// **The two emits produce the same containers and differ in what they cost.** Reading a
/// container's whole 1,024 words costs the same whether it holds 30,000 rows or three: a popcount
/// over 8 KB, a second pass to write the members out, and an 8 KB wipe. Following the marks instead
/// costs 16 words, plus one read and one clear of each word that holds a row, so it is cheaper
/// while a container's rows occupy under about a thousand of its words.
///
/// A bucket under this many rows averages fewer than one row per stamp word, so every container in
/// it is far inside that region. A session's mask is orders of magnitude above it — a quarter of
/// 10⁹ rows is a million rows per bucket — and takes the whole-container emit unchanged.
const SPARSE_BUCKET_ROWS: usize = STAMP_WORDS;

/// The buffers [`Permutation::project_with`] reuses between calls.
///
/// A 512 KB stamp, an 8 KB mark array over it, one `Vec` per bucket and one member run. They carry
/// no information between calls — the row buckets and the member run are cleared where they are
/// filled, and the stamp and its marks are all-zero on both entry and exit — so a `Default` one and
/// a reused one give byte-identical results; what reuse saves is the allocation and the zeroing,
/// which at one projection per artifact is the dominant cost of the artifact pass rather than a
/// rounding error.
#[derive(Default)]
pub struct ProjectScratch {
    buckets: Vec<Vec<u32>>,
    stamp: Vec<u64>,
    marks: Vec<u64>,
    members: Vec<u32>,
}

/// Emit the buckets' containers, union them into `out`, and leave the buckets empty for the next
/// window.
///
/// **The union is what windowing costs.** A whole-result bucket pass emits each container once, in
/// ascending key order, and one [`Sink`] stream is the answer; a windowed one emits each container
/// once per window that touched it, so the parts have to be merged. The parts are disjoint by
/// construction — each row is bucketed exactly once, by the one window whose entities reached it —
/// so the union is the same set the single pass produced, container representation aside.
///
/// The first window is moved into `out` rather than unioned with an empty bitmap, so a mask that
/// fits in one window pays nothing at all for the machinery.
fn emit_window(
    out: &mut croaring::Bitmap,
    buckets: &mut [Vec<u32>],
    stamp: &mut [u64],
    marks: &mut [u64],
    members: &mut Vec<u32>,
) {
    let mut sink = Sink::new();
    let mut emitted = false;
    for (index, rows) in buckets.iter().enumerate() {
        if rows.is_empty() {
            continue;
        }
        emitted = true;
        let base = (index as u32) << BUCKET_SHIFT;
        if rows.len() < SPARSE_BUCKET_ROWS {
            emit_sparse(&mut sink, stamp, marks, members, rows, base);
            continue;
        }
        // Which of the bucket's 64 containers hold anything. One `u64` covers them exactly,
        // which is what lets the emit below skip the empty ones without scanning their words.
        let mut occupied: u64 = 0;
        for &row in rows.iter() {
            let offset = row - base;
            stamp[(offset >> 6) as usize] |= 1u64 << (offset & 63);
            occupied |= 1u64 << (offset >> 16);
        }
        // Emitted and cleared in the same pass, container by container. Clearing *here* rather
        // than in a second loop is worth stating: the container's words are in cache because
        // the popcount just read them, and only the occupied ones are touched at all, so the
        // bucket pays a sequential 8 KB wipe per container it filled. The alternative that
        // looks frugal — re-walking each bucket's rows and zeroing the word each sits in — is
        // O(rows) rather than O(width), which sounds better and is 250 million scattered writes
        // at 10⁹ against 122 MB of sequential ones. Also not separately measured, and stated as
        // reasoning rather than as a result.
        for (container, words) in stamp.chunks_exact_mut(WORDS).enumerate() {
            if occupied & (1u64 << container) == 0 {
                continue;
            }
            let cardinality: u32 = words.iter().map(|word| word.count_ones()).sum();
            let key = u16::try_from((base >> 16) + container as u32)
                .expect("a row below 2^32 has a container key below 2^16");
            sink.push_block(
                key,
                cardinality,
                (&*words).try_into().expect("a bucket is whole containers"),
            );
            words.fill(0);
        }
    }
    if !emitted {
        return;
    }
    for bucket in buckets.iter_mut() {
        bucket.clear();
    }
    let part = sink.finish();
    if out.is_empty() {
        *out = part;
    } else {
        out.or_inplace(&part);
    }
}

/// Stamp one bucket's rows and emit the containers they landed in, reading only the stamp words
/// that hold one — [`Permutation::project_with`]'s emit for a bucket under [`SPARSE_BUCKET_ROWS`].
///
/// The mark array carries one bit per stamp word, so a container's members are read in one pass
/// over the words that hold them rather than three over all 1,024. Both arrays are cleared as they
/// are read, which is what leaves them zero for the next call.
///
/// The containers this stages are the ones the whole-container emit would have staged, with the
/// same keys, the same cardinalities and the same ascending members: [`Sink::push_members`] writes
/// the array payload [`Sink::push_block`] writes below the array threshold, and stamps the words
/// itself above it.
fn emit_sparse(
    sink: &mut Sink,
    stamp: &mut [u64],
    marks: &mut [u64],
    members: &mut Vec<u32>,
    rows: &[u32],
    base: u32,
) {
    let mut occupied: u64 = 0;
    for &row in rows {
        let offset = row - base;
        let word = (offset >> 6) as usize;
        stamp[word] |= 1u64 << (offset & 63);
        marks[word >> 6] |= 1u64 << (word & 63);
        occupied |= 1u64 << (offset >> 16);
    }
    for container in 0..CONTAINERS_PER_BUCKET {
        if occupied & (1u64 << container) == 0 {
            continue;
        }
        members.clear();
        let words_at = container * WORDS;
        let marks_at = container * MARKS_PER_CONTAINER;
        for slot in 0..MARKS_PER_CONTAINER {
            let mut marked = std::mem::replace(&mut marks[marks_at + slot], 0);
            while marked != 0 {
                let word = slot * 64 + marked.trailing_zeros() as usize;
                marked &= marked - 1;
                let mut bits = std::mem::replace(&mut stamp[words_at + word], 0);
                let low = (word as u32) * 64;
                while bits != 0 {
                    members.push(low + bits.trailing_zeros());
                    bits &= bits - 1;
                }
            }
        }
        let key = u16::try_from((base >> 16) + container as u32)
            .expect("a row below 2^32 has a container key below 2^16");
        sink.push_members(key, members);
    }
}

/// Entity IDs decoded from the mask at a time.
///
/// The point of decoding in bulk at all is that croaring's `read_many` is a memcpy per container
/// where stepping an iterator is a call per value: an alternative that range-splits the mask and
/// steps a cursor per range measured between 0.70× and 1.07× of this across scales — that is, never
/// reliably better, and it gives up the sequential walk of the slot array as well. The point of the
/// window being *small* is that it is reused: at 10⁹ a whole entity list is 1 GB written and
/// immediately re-read, and this is 32 KB that stays hot.
const DECODE_WINDOW: usize = 8192;

/// Row ids buffered in [`Permutation::project_with`]'s buckets before they are emitted and unioned
/// into the result — the bound on that pass's transient memory.
///
/// Bucketing the whole result before emitting any of it costs four bytes a projected row: 14 GB at
/// 3.5×10⁹ rows over a whole-corpus grant, a transient no configured budget bounds. Emitting a
/// window at a time makes the bucket transient a constant — 64 MB of row ids, plus the quarter of slack
/// [`Permutation::project_with`] reserves on top — and leaves the result itself as the only term
/// that grows with the grant.
///
/// The window's *size* barely moves the cost either way. The union it feeds is one insertion a row
/// whatever the window is; what a smaller window adds is one container merge per window per
/// container of the result, which beside the insertions is a rounding error. 64 MB is chosen to sit
/// far below any budget a serving box has and far above the size at which per-window overhead is
/// measurable.
const PROJECT_WINDOW_ROWS: usize = (64 << 20) / 4;

/// A memory-mapped `permutation.bin`. `row_of` and `project` are the only ways to cross from
/// entity space to row space anywhere in the codebase (I4) — no other module may open this
/// file or otherwise derive a row ID from an entity ID.
#[derive(Debug)]
pub struct Permutation {
    mmap: Mmap,
    bound: u64,
    bound_usize: usize,
    page_count: usize,
    present_count: usize,
    payload_start: usize,
    path: PathBuf,
    /// The row count this file's slots were found to claim exactly — recorded by
    /// [`Self::validate_rows`] when the slots it claimed numbered the whole of `[0, row_count)`,
    /// and left unset otherwise. [`Self::project_with`] reads it for the whole-domain case; the
    /// argument is there.
    dense_rows: std::sync::OnceLock<u32>,
}

impl Permutation {
    /// The permutation of a view that has no row space at all — **a view created while the
    /// service runs, before its first flush** (`views.md` §3.2).
    ///
    /// `bound = 0`, no pages, no present slots: [`Self::row_of`] answers `None` for every entity
    /// and [`Self::project`] answers the empty bitmap, which is what an empty view must answer.
    /// It is an anonymous mapping carrying the same header a `bound = 0` file would, rather than
    /// a second representation with its own arithmetic — every accessor below reads it exactly as
    /// it reads a mapped file, so there is one decode path and not two.
    ///
    /// **No file is written for it, and that is the point.** A created view owns nothing on disc
    /// until the flush that gives it rows; writing an empty `permutation.bin` into the live prefix
    /// at creation would put a file into a bundle the create does not otherwise publish, and a
    /// prefix rotation would then have to carry it.
    pub fn empty() -> Result<Self> {
        let len = payload_start(0);
        let mut map = memmap2::MmapOptions::new()
            .len(len)
            .map_anon()
            .map_err(|source| StoreError::Io {
                path: PathBuf::from("<empty permutation>"),
                source,
            })?;
        map[0..4].copy_from_slice(PERMUTATION_MAGIC);
        map[4..6].copy_from_slice(&PERMUTATION_VERSION.to_le_bytes());
        map[6..8].copy_from_slice(&(PAGE_SHIFT as u16).to_le_bytes());
        // bound, page_count and present_count are all zero, which the zeroed mapping already
        // holds; they are not written back so that the header's shape is stated once, above.
        let mmap = map.make_read_only().map_err(|source| StoreError::Io {
            path: PathBuf::from("<empty permutation>"),
            source,
        })?;
        Ok(Permutation {
            mmap,
            bound: 0,
            bound_usize: 0,
            page_count: 0,
            present_count: 0,
            payload_start: len,
            path: PathBuf::from("<empty permutation>"),
            dense_rows: std::sync::OnceLock::new(),
        })
    }

    /// Open and validate `path`: magic, version, page width, that the directory is canonical, and
    /// that the file is exactly as long as its own header says (a truncated or padded file is a
    /// corrupt bundle, not a partial one to silently accept).
    pub fn load(path: &Path) -> Result<Self> {
        let file = File::open(path).map_err(|source| StoreError::Io {
            path: path.to_path_buf(),
            source,
        })?;
        // SAFETY: the mapped file is treated as read-only for the lifetime of this struct;
        // nothing in this process writes to it concurrently. A backing file that another
        // process truncates while mapped is an operational hazard shared with every other
        // mmap-based reader in this codebase (`tessera-authz`'s postings reader), not one
        // introduced here.
        let mmap = unsafe { Mmap::map(&file) }.map_err(|source| StoreError::Io {
            path: path.to_path_buf(),
            source,
        })?;

        let invalid = |detail: String| StoreError::InvalidPermutation {
            path: path.to_path_buf(),
            detail,
        };

        if mmap.len() < HEADER_LEN {
            return Err(invalid(format!(
                "file is {} bytes, shorter than the {HEADER_LEN}-byte header",
                mmap.len()
            )));
        }
        if &mmap[0..4] != PERMUTATION_MAGIC {
            return Err(invalid("bad magic (expected 'TSPM')".to_string()));
        }
        let version = u16::from_le_bytes(mmap[4..6].try_into().expect("2-byte view"));
        if version != PERMUTATION_VERSION {
            // Version 1 is the flat array this representation replaced; it lands here, which is
            // the loud refusal that stands in for a `bundle_format` bump.
            return Err(invalid(format!(
                "unsupported version {version} (expected {PERMUTATION_VERSION})"
            )));
        }
        let page_shift = u16::from_le_bytes(mmap[6..8].try_into().expect("2-byte view"));
        if u32::from(page_shift) != PAGE_SHIFT {
            return Err(invalid(format!(
                "page shift {page_shift} (expected {PAGE_SHIFT})"
            )));
        }
        let bound = u64::from_le_bytes(mmap[8..16].try_into().expect("8-byte view"));
        // Checked, not `as usize`: on a 32-bit target (or an adversarial 64-bit `bound` value)
        // a truncating cast would silently shrink `bound` instead of failing closed.
        let bound_usize = usize::try_from(bound).map_err(|_| {
            invalid(format!(
                "bound {bound} does not fit in usize on this platform"
            ))
        })?;
        let page_count = u32::from_le_bytes(mmap[16..20].try_into().expect("4-byte view")) as usize;
        let present_count =
            u32::from_le_bytes(mmap[20..24].try_into().expect("4-byte view")) as usize;
        if page_count as u64 != pages_for(bound) {
            return Err(invalid(format!(
                "page count {page_count} does not cover bound {bound} (expected {})",
                pages_for(bound)
            )));
        }
        if present_count > page_count {
            return Err(invalid(format!(
                "{present_count} pages present of {page_count} covered"
            )));
        }

        let payload_start = payload_start(page_count);
        let expected_len = present_count
            .checked_mul(PAGE_BYTES)
            .and_then(|payload| payload.checked_add(payload_start))
            .ok_or_else(|| {
                invalid(format!(
                    "{present_count} pages of {PAGE_BYTES} bytes overflow the expected file length"
                ))
            })?;
        if mmap.len() != expected_len {
            return Err(invalid(format!(
                "file is {} bytes, expected {expected_len} for {present_count} present pages of \
                 {page_count}",
                mmap.len()
            )));
        }

        let permutation = Permutation {
            mmap,
            bound,
            bound_usize,
            page_count,
            present_count,
            payload_start,
            path: path.to_path_buf(),
            dense_rows: std::sync::OnceLock::new(),
        };
        permutation.validate_directory()?;
        Ok(permutation)
    }

    /// The directory is canonical: slots number `0..present_count` in ascending page order, the
    /// padding between it and the payload is zero, and the last page holds nothing above `bound`.
    ///
    /// **Checked because the encoding claims to be a function of the mapping.** A file whose slots
    /// were permuted would serve every page under some other page's rows — every lookup wrong, no
    /// lookup out of range — and two producers of the same mapping could disagree byte for byte
    /// while both being "valid". `O(page_count)`, which is 65 536 iterations at the entity ceiling.
    fn validate_directory(&self) -> Result<()> {
        let invalid = |detail: String| StoreError::InvalidPermutation {
            path: self.path.clone(),
            detail,
        };
        let mut next_slot: u32 = 0;
        for (page, &slot) in self.directory().iter().enumerate() {
            if slot == PAGE_ABSENT {
                continue;
            }
            if slot != next_slot {
                return Err(invalid(format!(
                    "page {page} holds slot {slot} where the canonical order gives {next_slot}"
                )));
            }
            next_slot += 1;
        }
        if next_slot as usize != self.present_count {
            return Err(invalid(format!(
                "the directory names {next_slot} pages, the header {}",
                self.present_count
            )));
        }
        let padding = &self.mmap[HEADER_LEN + self.page_count * 4..self.payload_start];
        if padding.iter().any(|&b| b != 0) {
            return Err(invalid(
                "the padding before the payload is not zero".to_string(),
            ));
        }
        // The last page runs past `bound` whenever the bound is not a whole number of pages. Those
        // slots name entities that cannot exist, so they must be sentinel: a row id there would be
        // reachable through nothing and would break the bijection `validate_rows` checks.
        let tail = (self.page_count * PAGE_ENTRIES) - self.bound_usize;
        if tail > 0 {
            if let Some(page) = self.page_of(self.page_count - 1) {
                if page[PAGE_ENTRIES - tail..].iter().any(|&r| r != ROW_ABSENT) {
                    return Err(invalid(format!(
                        "the last page holds a row above bound {}",
                        self.bound
                    )));
                }
            }
        }
        Ok(())
    }

    /// The number of entity-ID slots this permutation covers, `[0, bound)`.
    pub fn bound(&self) -> u64 {
        self.bound
    }

    /// How many pages carry slots — the sparsity of the view, and what the file's size is
    /// proportional to. For diagnostics and for the tests that assert a sparse view does not pay
    /// for entity space it does not occupy.
    pub fn present_pages(&self) -> usize {
        self.present_count
    }

    /// How many pages `bound` spans, present or not.
    pub fn page_count(&self) -> usize {
        self.page_count
    }

    fn directory(&self) -> &[u32] {
        let bytes = &self.mmap[HEADER_LEN..HEADER_LEN + self.page_count * 4];
        // SAFETY: `bytes` starts at a fixed offset (HEADER_LEN = 24) into a page-aligned mmap
        // base, and 24 is a multiple of 4, so the cast is aligned regardless of file content — no
        // adversarial input can misalign it. The length is `page_count * 4` bytes, and `load`
        // checked that the file holds at least the header, the directory and the payload.
        unsafe { std::slice::from_raw_parts(bytes.as_ptr() as *const u32, self.page_count) }
    }

    /// The `2^16` slots of `page`, or `None` where the page is absent.
    #[inline]
    fn page_of(&self, page: usize) -> Option<&[u32]> {
        let slot = *self.directory().get(page)?;
        if slot == PAGE_ABSENT {
            return None;
        }
        let at = self.payload_start + slot as usize * PAGE_BYTES;
        let bytes = &self.mmap[at..at + PAGE_BYTES];
        // SAFETY: `payload_start` is a multiple of 4096 and `PAGE_BYTES` a multiple of 4, so the
        // cast is aligned; `validate_directory` bounded `slot` by `present_count` and `load`
        // checked the file holds exactly that many pages.
        Some(unsafe { std::slice::from_raw_parts(bytes.as_ptr() as *const u32, PAGE_ENTRIES) })
    }

    /// Validate that every non-sentinel slot addresses a row within `row_count`, and that no
    /// two entities claim the same row (a permutation is a bijection onto `[0, row_count)`,
    /// not merely a function into it). Called once per view at bundle open, against the row
    /// count of the single build segment this permutation addresses (R4) — **not** on any
    /// per-viewport path (this is an `O(present pages)` scan, same cost class as [`Self::project`]).
    /// A corrupt or hand-edited `permutation.bin` that points rows out of range, or that
    /// aliases two entities onto one row, must fail bundle open rather than let `row_of` or
    /// `project` later hand out a `RowId` that indexes `columns.arrow` out of bounds (I4/I11).
    ///
    /// **It also records whether the mapping is onto `[0, row_count)` and not merely into it.**
    /// The checks below establish injectivity; counting the claims is what turns that into
    /// surjectivity, since `row_count` distinct claims below `row_count` are all of them. Only the
    /// surjective case is recorded, and only a recorded one enables [`Self::project_with`]'s
    /// whole-domain answer — a file claiming fewer rows than the descriptor declares takes the
    /// general path, where the image is read rather than assumed.
    pub fn validate_rows(&self, row_count: u32) -> Result<()> {
        let row_count_usize = row_count as usize;
        let mut seen = RowsSeen::new(row_count_usize);
        let mut claimed: u64 = 0;
        for page in 0..self.page_count {
            let Some(slots) = self.page_of(page) else {
                continue;
            };
            for (offset, &slot) in slots.iter().enumerate() {
                if slot == ROW_ABSENT {
                    continue;
                }
                let entity = page * PAGE_ENTRIES + offset;
                if slot >= row_count {
                    return Err(StoreError::InvalidPermutation {
                        path: self.path.clone(),
                        detail: format!(
                            "entity {entity} maps to row {slot}, out of bound for row_count \
                             {row_count}"
                        ),
                    });
                }
                if seen.claim(slot as usize) {
                    return Err(StoreError::InvalidPermutation {
                        path: self.path.clone(),
                        detail: format!(
                            "row {slot} is claimed by more than one entity (not a bijection)"
                        ),
                    });
                }
                claimed += 1;
            }
        }
        if claimed == u64::from(row_count) {
            let _ = self.dense_rows.set(row_count);
        }
        Ok(())
    }

    /// Every entity that holds a row here, ascending, with the row it holds.
    ///
    /// **The route across a whole permutation**, where [`Self::row_of`] is the route to one
    /// entity. A sweep by `row_of` repeats the directory lookup at every slot and visits each
    /// entity of an absent page one at a time; this reads the directory once a page and each
    /// present page end to end, so a sparse view costs its own slots and not its entity span.
    /// `tessera verify` crosses entity space this way.
    pub fn try_for_each_slot<E>(
        &self,
        mut f: impl FnMut(u64, RowId) -> std::result::Result<(), E>,
    ) -> std::result::Result<(), E> {
        for page in 0..self.page_count {
            let Some(slots) = self.page_of(page) else {
                continue;
            };
            for (offset, &slot) in slots.iter().enumerate() {
                let entity = ((page as u64) << PAGE_SHIFT) | offset as u64;
                // The last page covers `bound` rounded up, so its tail slots address entities
                // this permutation does not have. `row_of` refuses them and so does this.
                if entity >= self.bound {
                    return Ok(());
                }
                if slot == ROW_ABSENT {
                    continue;
                }
                f(entity, RowId::new(slot))?;
            }
        }
        Ok(())
    }

    /// Row ID currently occupied by `e` in this segment, or `None` if `e` is out of bound, falls
    /// in an absent page, or holds the row-absent sentinel (never allocated a row here — including
    /// entities that exist but live in a different segment, or don't exist at all).
    pub fn row_of(&self, e: EntityId) -> Option<RowId> {
        let raw = e.raw();
        if raw >= self.bound {
            return None;
        }
        let page = self.page_of((raw >> PAGE_SHIFT) as usize)?;
        let slot = page[(raw as usize) & (PAGE_ENTRIES - 1)];
        if slot == ROW_ABSENT {
            None
        } else {
            Some(RowId::new(slot))
        }
    }

    /// Project an entity-space bitmap into row space: for every entity ID set in `mask`
    /// (ascending order, as `croaring::Bitmap` iterates), look up its row via this
    /// permutation, skip entities with no row here (out of bound, absent page, or sentinel), and
    /// return the resulting row IDs as a bitmap.
    ///
    /// **Cost (shared-context constraint 8):** this touches every set bit in `mask` and reads the
    /// pages it lands in end to end, which is over a second at 10⁹ — **1 277 ms** single-threaded
    /// over a 25% grant (`probes/2026-08-14-project-decomposition/`, measured against the flat
    /// array this paged form replaces; a dense view's page walk is the same reads plus a directory
    /// lookup per 2¹⁶ entities, and a sparse view's is strictly less). Never call it on the
    /// per-viewport path; the engine caches the result per `(token, view, pin)` and reuses it
    /// across viewports within a session.
    ///
    /// # One pass, and why the shape is not the obvious one
    ///
    /// §10.4 asks for "gather, radix sort, and bulk-construct from the sorted array". The gather
    /// and the sort are the same pass here, and the sort is not a sort.
    ///
    /// A row is written **once**, into a bucket, straight out of the slot lookup — the entity list
    /// never exists, and neither does a row array to be sorted and re-read. The earlier form wrote
    /// it four times over (decode to a `Vec`, gather, sort in place, read back through `add_many`)
    /// and spent 5 272 ms of its 8 267 ms in `par_sort_unstable` alone.
    ///
    /// **A bucket is a row range, not a container key**, and that is the load-bearing choice rather
    /// than an arbitrary one. Partitioning on the high 16 bits — which *is* the Roaring container
    /// key, so it looks like the natural unit — leaves 15 259 live write cursors at 10⁹, far more
    /// than the cache holds, and was **measured to get worse with scale**: 3.22× over the stages it
    /// replaces at 10⁸ but only 2.06× at 10⁹. [`BUCKET_SHIFT`] instead fixes the two things that
    /// decide the cost, and they pull in opposite directions: few enough buckets that the write
    /// cursors stay in L1, wide enough that the bit array each one stamps stays in L2.
    ///
    /// **Containers are emitted, not inserted.** The bit array a bucket stamps *is* 64 Roaring
    /// container payloads laid end to end, so [`tessera_roaring::Sink`] takes them as they are —
    /// a popcount and a memcpy each. Expanding them back to `u32` for `add_many` instead costs
    /// 1 077 ms more at 10⁹ (2 306 ms against 1 229 ms), which is the larger half of the primitive.
    ///
    /// **Serial, deliberately, and it is still faster in wall clock.** The form this replaces was
    /// rayon-parallel and reached **3 024 ms on twelve threads** at 10⁹; this reaches 1 277 ms on
    /// one. So the serial rewrite is not a trade of latency for efficiency — it wins both, by 2.4×
    /// on wall clock while leaving eleven cores to other sessions. That second part is the reason
    /// to care: a server answering concurrent sessions is already saturated, so a projection that
    /// spreads across cores buys throughput nothing and only removing work counts.
    ///
    /// The one-pass form has no parallel decomposition worth taking in any case — see
    /// [`DECODE_WINDOW`] for the range-split alternative and why bulk decode beats it.
    ///
    /// **Transient memory is [`PROJECT_WINDOW_ROWS`] row ids, whatever the grant.** The buckets
    /// hold one window at a time: they are emitted into the result and cleared each time they fill,
    /// so the 4 bytes a projected row that a whole-result bucket pass costs — 14 GB at 3.5×10⁹ rows
    /// over a whole-corpus grant — is 64 MB and does not move with the mask. What still scales with
    /// the grant is the result, which is the answer. The mmap-backed pages are never copied, only
    /// read.
    ///
    /// **A mask holding every entity in `[0, bound)` is answered without reading a page.** Its
    /// image is every row this permutation has, and [`Self::validate_rows`] records at bundle open
    /// when those are exactly `[0, row_count)` — so the answer is that range, built directly. It is
    /// the same set the general path returns from the same mask, so nothing a caller can observe
    /// differs: the projection is a permutation applied to a mask, and a mask over the whole domain
    /// maps onto the whole range under every permutation (I10). The mapping itself is therefore not
    /// consulted, and a caller learns nothing about it that the row count did not already say.
    pub fn project(&self, mask: &croaring::Bitmap) -> croaring::Bitmap {
        self.project_with(mask, &mut ProjectScratch::default())
    }

    /// [`Self::project`], reusing a caller's scratch buffers.
    ///
    /// **For a caller that projects many masks in a row**, which the artifact pass does — once per
    /// artifact, three times over per level. The scratch this reuses is a 512 KB stamp plus one
    /// `Vec` per bucket, and allocating it per call put an `mmap`/`munmap` pair and 128 minor
    /// faults on every projection: at 6×10⁵ artifacts that is ~10⁸ page faults and hundreds of
    /// gigabytes of zeroing, for buffers whose contents never outlive the call. The session path
    /// projects once and keeps [`Self::project`], which allocates as it always did.
    ///
    /// Behaviour is identical — the buffers are cleared rather than carried, exactly as a fresh
    /// allocation would leave them.
    pub fn project_with(
        &self,
        mask: &croaring::Bitmap,
        scratch: &mut ProjectScratch,
    ) -> croaring::Bitmap {
        if let Some(&rows) = self.dense_rows.get() {
            if self.covers_domain(mask) {
                return match rows {
                    0 => croaring::Bitmap::new(),
                    rows => croaring::Bitmap::from_range(0..rows),
                };
            }
        }
        self.project_windowed(mask, scratch, PROJECT_WINDOW_ROWS)
    }

    /// [`Self::project_with`]'s general path: the bucket walk, emitting and unioning every
    /// `window_rows` rows.
    ///
    /// **The window is a parameter because the two forms have to be compared.** A windowed pass and
    /// a single one must produce the same set, and the production window is 16.7 million rows —
    /// larger than any fixture a test builds. Passing `usize::MAX` gives the single-pass form, which
    /// is what the equality is checked against; [`Self::project_with`] passes
    /// [`PROJECT_WINDOW_ROWS`].
    fn project_windowed(
        &self,
        mask: &croaring::Bitmap,
        scratch: &mut ProjectScratch,
        window_rows: usize,
    ) -> croaring::Bitmap {
        // Every row this permutation can yield is below `bound`: `validate_rows` establishes that
        // it is a bijection *onto* `[0, row_count)`, so each row is claimed by a distinct in-bound
        // entity and `row_count <= bound`. Sizing the buckets from `bound` therefore cannot
        // under-count them, whatever the mask contains.
        let nbuckets = (self.bound_usize >> BUCKET_SHIFT) + 1;
        // Rows land near-uniformly across row space — a build orders them by `(morton,
        // tessera_id)`, which is uncorrelated with entity-issue order — so the mean plus a quarter
        // absorbs the variation without a histogram pass to find it. **The slack is not a rounding
        // habit**: reserving the bare mean leaves about half the buckets to exceed it and double,
        // and a bucket at 10⁹ is megabytes, so that is a realloc and a memcpy of the whole thing on
        // half of them. Not separately measured — it was not separable from run-to-run variance at
        // this size — so it is here on the argument, not on a number.
        // Sized from the window rather than from the mask, because a window is all a bucket ever
        // holds. A mask below one window reserves for the mask instead, so a small projection
        // reserves what it needs and no more.
        let planned = (mask.cardinality() as usize).min(window_rows);
        let expected = (planned / nbuckets)
            .saturating_mul(5)
            .saturating_div(4)
            .saturating_add(64);
        let buckets = &mut scratch.buckets;
        for bucket in buckets.iter_mut() {
            bucket.clear();
        }
        buckets.resize_with(nbuckets, Vec::new);
        buckets.truncate(nbuckets);
        for bucket in buckets.iter_mut() {
            bucket.reserve(expected.saturating_sub(bucket.capacity()));
        }

        // **Zeroed when it is sized and not again**, because both emits below clear every word they
        // set. The whole-container one fills each occupied container as it stages it; the sparse
        // one clears each stamp word and each mark word as it reads them. A scratch that has been
        // through a projection therefore comes back all-zero, and re-zeroing it is 512 KB of stores
        // on every call — the whole of a projection's cost for the small memberships an artifact
        // level is mostly made of (`probes/2026-09-09-layers-cost/`).
        let stamp = &mut scratch.stamp;
        if stamp.len() != STAMP_WORDS {
            stamp.clear();
            stamp.resize(STAMP_WORDS, 0);
        }
        let marks = &mut scratch.marks;
        if marks.len() != MARK_WORDS {
            marks.clear();
            marks.resize(MARK_WORDS, 0);
        }
        let members = &mut scratch.members;
        debug_assert!(
            stamp.iter().chain(marks.iter()).all(|word| *word == 0),
            "both emits clear each word they write, so the stamp and its marks are zero on entry"
        );

        let mut rows = croaring::Bitmap::new();
        // Rows in the buckets. The emit below runs whenever this reaches a window and starts it
        // again from nothing, so the buckets never hold more than one window plus the tail of the
        // decode block that filled them.
        let mut buffered = 0usize;
        // The page the last entity landed in, held across the walk. A mask iterates ascending, so
        // this resolves the directory once per page rather than once per entity — and an absent
        // page is skipped as cheaply as a sentinel slot was.
        let mut current: Option<(usize, &[u32])> = None;
        let mut window = [0u32; DECODE_WINDOW];
        let mut cursor = mask.cursor();
        loop {
            let decoded = cursor.read_many(&mut window);
            if decoded == 0 {
                break;
            }
            for &entity in &window[..decoded] {
                // An entity at or above `bound` has no row *here* and is skipped, which is the
                // same answer `row_of` gives and is not an error — it is ordinarily an entity
                // living in a different segment.
                if u64::from(entity) >= self.bound {
                    continue;
                }
                let page = (entity >> PAGE_SHIFT) as usize;
                if current.map(|(p, _)| p) != Some(page) {
                    current = self.page_of(page).map(|slots| (page, slots));
                    if current.is_none() {
                        // Absent, and the mask may hold thousands more entities in it. Recording
                        // the miss is what stops the directory being re-read for each of them.
                        current = Some((page, &[]));
                    }
                }
                let Some((_, slots)) = current else { continue };
                if slots.is_empty() {
                    continue;
                }
                let row = slots[(entity as usize) & (PAGE_ENTRIES - 1)];
                if row != ROW_ABSENT {
                    buckets[(row >> BUCKET_SHIFT) as usize].push(row);
                    buffered += 1;
                }
            }
            if buffered >= window_rows {
                emit_window(&mut rows, buckets, stamp, marks, members);
                buffered = 0;
            }
        }
        emit_window(&mut rows, buckets, stamp, marks, members);
        rows
    }

    /// Whether `mask` holds every entity this permutation covers, `[0, bound)`.
    ///
    /// `O(containers)` over entity space, and the first gap answers it — 65 536 containers at the
    /// entity ceiling, against the pass it decides whether to run.
    fn covers_domain(&self, mask: &croaring::Bitmap) -> bool {
        let Some(hi) = self.bound.checked_sub(1) else {
            // No entities, so every mask holds all of them.
            return true;
        };
        match u32::try_from(hi) {
            Ok(hi) => mask.contains_range(0..=hi),
            // An entity id is a `u32` (I9), so a bound above the ceiling names entities no mask
            // can hold. The general path answers it.
            Err(_) => false,
        }
    }

    /// The path this permutation was loaded from (for diagnostics only).
    pub fn path(&self) -> &Path {
        &self.path
    }
}

/// One flush or merge segment's slice of row space: the entity range it covers, and where in
/// row space its rows live.
///
/// `rows` is **dense over `[entity_lo, entity_hi]`** and holds each entity's row *relative to*
/// `row_base`, or [`ROW_ABSENT`] for an entity the segment never got a row for — a
/// deleted-at-flush entity, whose ID stays burned (I9) while no row is created for it. Dense
/// rather than sparse because the range is contiguous by construction: entity IDs are issued
/// monotonically from the high-water, so a flush segment covers a contiguous ascending range
/// (§2.1), and one `u32` per entity is smaller than any keyed form over the same span.
///
/// It is not a `permutation.bin`. That file's length is the *bundle's* whole entity space, which
/// is the wrong shape for a segment covering a few thousand ids at the top of it; an extent's rows
/// live in the side-manifest's file set instead.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SegmentExtent {
    pub entity_lo: u64,
    /// Inclusive.
    pub entity_hi: u64,
    pub seg_id: String,
    pub row_base: u32,
    /// `rows[e - entity_lo]`, relative to `row_base`; [`ROW_ABSENT`] where the entity has no row.
    pub rows: Vec<u32>,
}

impl SegmentExtent {
    /// Recover a flush or merge segment's extent from the segment itself, at open.
    ///
    /// **Why this exists rather than a file.** §2.1 describes an extent as four scalars —
    /// `{entity_lo, entity_hi, seg_id, row_base}` — which is only a mapping if the segment's rows
    /// are in entity order. The same section requires every segment to be Morton-sorted, and that
    /// sort *is* the tile index, so the mapping is an arbitrary permutation of the entity range and
    /// four scalars cannot express it. Something has to carry it across a restart.
    ///
    /// The alternative was to write `rows` beside `morton.u32` (4 bytes × entities in the segment,
    /// one more file, one more manifest field, a contracts §2.1 change). This construction stores
    /// **nothing**: `columns.arrow` already carries `tessera_id` at the row, the identity is a
    /// bijection over 2⁶⁴ ([`tessera_types::IdentityKey`]), and `MANIFEST.json` already carries the
    /// key — so the mapping is derivable from artefacts that must exist anyway. Ruled on
    /// 2026-08-02 in favour of adding no artefact.
    ///
    /// **The invariant it spends, stated so it is not spent again silently.** Row space above the
    /// build bound is now recoverable *only* while the identity permutation is invertible at open.
    /// A future construction that made `tessera_id` one-way — a keyed hash, a key held outside the
    /// bundle, a per-session blinding — would silently strand every flushed entity's row, exactly
    /// the failure this replaces. I10 is unaffected: nothing here leaves the engine, and the
    /// inversion is the same one `Engine::item` already performs per request.
    ///
    /// **Cost.** One Feistel inversion per row of each flush segment, at every open, bounded by
    /// what merge leaves unmerged rather than by the corpus. Deliberately not parallelised: it runs
    /// once at open, inside a loop that is already mapping and digest-verifying files.
    pub fn rebuild(
        seg_id: &str,
        entity_lo: u64,
        entity_hi: u64,
        row_base: u32,
        tessera_ids: &[u64],
        key: &tessera_types::IdentityKey,
        shard_id: u32,
    ) -> Result<Self> {
        let malformed = |detail: String| StoreError::MalformedBundle { detail };
        let span = entity_hi
            .checked_sub(entity_lo)
            .and_then(|d| d.checked_add(1))
            .and_then(|d| usize::try_from(d).ok())
            .ok_or_else(|| {
                malformed(format!(
                    "segment '{seg_id}': entity span {entity_lo}..={entity_hi} is inverted or too \
                     wide to address"
                ))
            })?;

        let mut rows = vec![ROW_ABSENT; span];
        for (local, &raw) in tessera_ids.iter().enumerate() {
            let (shard, entity) = key.invert(tessera_types::TesseraId::new(raw));
            // A wrong shard means this segment was written under a different identity
            // configuration than the manifest declares — corruption, not a row to skip. Serving
            // past it would put a row under an entity id that names a different item.
            if shard != shard_id {
                return Err(malformed(format!(
                    "segment '{seg_id}': row {local}'s tessera_id inverts to shard {shard}, but \
                     the manifest declares shard {shard_id}"
                )));
            }
            let raw_entity = entity.raw();
            if raw_entity < entity_lo || raw_entity > entity_hi {
                return Err(malformed(format!(
                    "segment '{seg_id}': row {local} belongs to entity {raw_entity}, outside the \
                     descriptor's range {entity_lo}..={entity_hi}"
                )));
            }
            let slot = &mut rows[(raw_entity - entity_lo) as usize];
            if *slot != ROW_ABSENT {
                return Err(malformed(format!(
                    "segment '{seg_id}': entity {raw_entity} is claimed by rows {} and {local} — \
                     the identity is a bijection, so two rows cannot invert to one entity",
                    *slot
                )));
            }
            *slot = local as u32;
        }

        let extent = SegmentExtent {
            entity_lo,
            entity_hi,
            seg_id: seg_id.to_string(),
            row_base,
            rows,
        };
        // The same check `with_extent` applies to an extent arriving from a flush. Applied here
        // too rather than left to the caller, so a rebuild that produced a malformed extent says
        // which segment it was reading rather than surfacing as "does not continue row space".
        if !extent.is_well_formed() {
            return Err(malformed(format!(
                "segment '{seg_id}': the extent rebuilt from its tessera_id column is not a \
                 bijection onto its own rows"
            )));
        }
        Ok(extent)
    }

    /// How many rows this extent actually owns — the non-absent slots, not the entity span.
    pub fn row_count(&self) -> u32 {
        self.rows.iter().filter(|&&r| r != ROW_ABSENT).count() as u32
    }

    /// Whether this extent is internally well-formed: its `rows` cover its entity span exactly,
    /// and the non-absent slots are a bijection onto `[0, row_count)`.
    ///
    /// The same property [`Permutation::validate_rows`] enforces for the base, and for the same
    /// reason: a slot outside the range, or two entities aliased onto one row, would let `row_of`
    /// hand out a `RowId` that indexes the segment's `columns.arrow` out of bounds (I4/I11).
    fn is_well_formed(&self) -> bool {
        if self.entity_hi < self.entity_lo {
            return false;
        }
        let Some(span) = self
            .entity_hi
            .checked_sub(self.entity_lo)
            .and_then(|d| d.checked_add(1))
            .and_then(|d| usize::try_from(d).ok())
        else {
            return false;
        };
        if self.rows.len() != span {
            return false;
        }
        let count = self.row_count();
        let mut seen = RowsSeen::new(count as usize);
        for &row in &self.rows {
            if row == ROW_ABSENT {
                continue;
            }
            if row >= count || seen.claim(row as usize) {
                return false;
            }
        }
        true
    }

    /// This extent's contribution to a projection: the rows of every entity of `mask` that falls
    /// inside it. Nothing else in `mask` can be answered here, so nothing else is looked at.
    ///
    /// **It seeks to `entity_lo` rather than skipping up to it, and the difference is the whole
    /// cost of a flush's patch.** An extent's entities are the newest in the partition, so they
    /// sit at the top of the mask; a walk from the mask's start pays O(mask cardinality) to reach
    /// them — *measured* 79 ms per extent against a 25 M-entity grant at 10⁸
    /// (`probes/2026-08-04-refresh-ladder/`), which is the patch's cost being a function of the
    /// grant's width rather than of the flush's size. `reset_at_or_after` costs O(containers
    /// skipped), which is the cost model this index is designed against
    /// (`CLAUDE.md`: bitmap operations cost O(containers touched), not O(cardinality)).
    fn project(&self, mask: &croaring::Bitmap) -> croaring::Bitmap {
        let mut rows: Vec<u32> = Vec::new();
        // `entity_hi` is inclusive and the range end is exclusive; both ends are already inside
        // `u32` because a `mask` is entity-space and entity ids are capped at `u32::MAX` (I9).
        let lo = u32::try_from(self.entity_lo).unwrap_or(u32::MAX);
        let hi = u32::try_from(self.entity_hi).unwrap_or(u32::MAX);
        let mut iter = mask.iter();
        iter.reset_at_or_after(lo);
        for entity in iter {
            if entity > hi {
                break;
            }
            let slot = self.rows[(entity - lo) as usize];
            if slot != ROW_ABSENT {
                rows.push(self.row_base + slot);
            }
        }
        croaring::Bitmap::of(&rows)
    }

    fn row_of(&self, entity: u64) -> Option<RowId> {
        if entity < self.entity_lo || entity > self.entity_hi {
            return None;
        }
        let slot = self.rows[(entity - self.entity_lo) as usize];
        (slot != ROW_ABSENT).then(|| RowId::new(self.row_base + slot))
    }
}

/// One view's whole entity→row mapping: the built base permutation, plus the extents flush has
/// appended and merge has collapsed since.
///
/// **Constructed incrementally, never rebuilt.** [`Self::with_extent`] and [`Self::collapsing`]
/// return a new value sharing the base by `Arc`, because re-opening it would re-pay
/// `Permutation::load`'s `O(bound)` `validate_rows` — more than the flush that prompted it.
///
/// The extent list is ordered, ascending and disjoint, and each extent begins exactly where row
/// space currently ends. That is what makes `total_rows` a running sum rather than a scan, and it
/// is what a merge preserves: a merge emits exactly as many rows as it consumed, so no later
/// extent's `row_base` ever moves.
#[derive(Debug, Clone)]
pub struct RowSpace {
    base: Arc<Permutation>,
    /// `row-entity.u32` for the base, when the view published one — the row→entity direction, for
    /// the filtered viewport's per-tile route ([`crate::row_entity`]). `None` where a view has no
    /// table, in which case [`Self::entity_of`] answers `None` and the caller falls back to the
    /// projecting route rather than to a wrong answer.
    base_inverse: Option<Arc<crate::row_entity::RowToEntity>>,
    /// The same direction for the rows *above* the base, derived from the extents' own mappings on
    /// first use and thrown away whenever an extent is added. Materialised rather than searched
    /// because the caller is a per-row path; bounded by the flushed tail, which the merge ladder
    /// bounds in turn.
    extent_inverse: std::sync::OnceLock<Vec<u32>>,
    /// The build segment's row count — the base owns `[0, base_rows)`. Not derivable from the
    /// permutation, whose `bound` is an entity-space width and may exceed its row count.
    base_rows: u32,
    /// Ordered, ascending, disjoint.
    extents: Vec<SegmentExtent>,
    /// `base_rows` plus every extent's `row_count`, maintained rather than recomputed.
    total_rows: u64,
}

impl RowSpace {
    pub fn new(base: Arc<Permutation>, base_rows: u32) -> Self {
        RowSpace {
            base,
            base_inverse: None,
            extent_inverse: std::sync::OnceLock::new(),
            base_rows,
            extents: Vec::new(),
            total_rows: base_rows as u64,
        }
    }

    /// This row space with the base's `row-entity.u32` attached.
    ///
    /// Additive rather than a constructor parameter: a row space is correct without it — every
    /// caller that only crosses entity→row is unaffected — and the table is an optimisation for
    /// the one path that crosses the other way.
    pub fn with_row_entity(mut self, table: Arc<crate::row_entity::RowToEntity>) -> Self {
        self.base_inverse = Some(table);
        self
    }

    /// Can this row space cross row→entity at all?
    ///
    /// A caller choosing between the per-tile and projecting routes asks this once, before
    /// committing to a route, rather than discovering row by row that [`Self::entity_of`] cannot
    /// answer. True when the base published a `row-entity.u32` — or when there is no base to
    /// invert, every row belonging to an extent, whose mapping is always recoverable.
    pub fn can_invert(&self) -> bool {
        self.base_inverse.is_some() || self.base_rows == 0
    }

    /// The entity occupying `row`, or `None` when this row space cannot answer — either `row` is
    /// out of range, or the view published no `row-entity.u32` and the base cannot be inverted
    /// without one.
    ///
    /// **`None` is "ask another way", not "no entity".** Every row has an entity by construction;
    /// a `None` here means the caller must fall back to the projecting route, and treating it as an
    /// absence would silently drop rows from a filtered viewport.
    pub fn entity_of(&self, row: RowId) -> Option<EntityId> {
        let raw = row.raw();
        if raw >= self.total_rows as u32 {
            return None;
        }
        if raw < self.base_rows {
            return self.base_inverse.as_ref()?.entity_of(row);
        }
        let table = self
            .extent_inverse
            .get_or_init(|| self.build_extent_inverse());
        table
            .get((raw - self.base_rows) as usize)
            .map(|&e| EntityId::new(e as u64))
    }

    /// Invert every extent's `rows` into one flat table covering `[base_rows, total_rows)`.
    ///
    /// Extents are disjoint and their `row_base`s continue row space exactly (`with_extent`
    /// enforces both), so the flat table is dense and each extent writes only its own span.
    fn build_extent_inverse(&self) -> Vec<u32> {
        let span = (self.total_rows - self.base_rows as u64) as usize;
        let mut out = vec![0u32; span];
        for extent in &self.extents {
            for (offset, &row) in extent.rows.iter().enumerate() {
                if row == ROW_ABSENT {
                    continue;
                }
                let absolute = extent.row_base as u64 + row as u64;
                let at = (absolute - self.base_rows as u64) as usize;
                out[at] = (extent.entity_lo + offset as u64) as u32;
            }
        }
        out
    }

    /// This row space plus one more segment, sharing the base.
    ///
    /// `None` if `extent` is malformed, does not begin strictly above the last extent's
    /// `entity_hi`, does not begin at or above the base's bound, or does not continue row space
    /// exactly (`row_base == total_rows()`). Every one of those is corruption rather than a state
    /// to tolerate: a gap or an overlap makes some other segment's rows unreachable or aliased.
    pub fn with_extent(&self, extent: SegmentExtent) -> Option<Self> {
        if !extent.is_well_formed() {
            return None;
        }
        let entity_floor = match self.extents.last() {
            Some(last) => last.entity_hi + 1,
            None => self.base.bound(),
        };
        if extent.entity_lo < entity_floor {
            return None;
        }
        if u64::from(extent.row_base) != self.total_rows {
            return None;
        }
        let total_rows = self.total_rows + u64::from(extent.row_count());
        // Row ids are `u32` (bundle_format 1), so a view that would cross 2^32 rows must fail
        // here rather than at the first `row_base + slot` that wraps.
        if total_rows > u64::from(u32::MAX) {
            return None;
        }
        let mut extents = self.extents.clone();
        extents.push(extent);
        Some(RowSpace {
            base: Arc::clone(&self.base),
            // The base table survives — the base's rows are exactly what an extent does not touch.
            // The derived tail does not: it covers the extents, and this call changes them.
            base_inverse: self.base_inverse.clone(),
            extent_inverse: std::sync::OnceLock::new(),
            base_rows: self.base_rows,
            extents,
            total_rows,
        })
    }

    /// This row space with the adjacent run `seg_ids` replaced by the single extent `merged`.
    ///
    /// `None` if any input `seg_id` is absent from the current generation — which is how a merge
    /// planned against a superseded generation is discarded rather than published. It is ABA-safe
    /// because `seg_id`s are never reused, across merges or prefixes (contracts §2.1), so a
    /// present `seg_id` is the same segment the merge consumed.
    ///
    /// Also `None` unless the inputs form a *contiguous run* and `merged` covers exactly their
    /// entity range at exactly their `row_base` and row count. A merge is row-count preserving, so
    /// anything else would move a later extent's rows.
    pub fn collapsing(&self, seg_ids: &[String], merged: SegmentExtent) -> Option<Self> {
        if seg_ids.is_empty() || !merged.is_well_formed() {
            return None;
        }
        let start = self
            .extents
            .iter()
            .position(|e| e.seg_id == seg_ids[0])
            .filter(|&i| i + seg_ids.len() <= self.extents.len())?;
        let run = &self.extents[start..start + seg_ids.len()];
        if run.iter().zip(seg_ids).any(|(e, id)| &e.seg_id != id) {
            return None;
        }
        let consumed_rows: u32 = run.iter().map(SegmentExtent::row_count).sum();
        if merged.entity_lo != run[0].entity_lo
            || merged.entity_hi != run[run.len() - 1].entity_hi
            || merged.row_base != run[0].row_base
            || merged.row_count() != consumed_rows
        {
            return None;
        }
        let mut extents = Vec::with_capacity(self.extents.len() - seg_ids.len() + 1);
        extents.extend_from_slice(&self.extents[..start]);
        extents.push(merged);
        extents.extend_from_slice(&self.extents[start + seg_ids.len()..]);
        Some(RowSpace {
            base: Arc::clone(&self.base),
            base_inverse: self.base_inverse.clone(),
            // Row-count preserving, but not extent-preserving: a merge replaces a run of extents
            // with one, so the tail this derived is stale even though its length is not.
            extent_inverse: std::sync::OnceLock::new(),
            base_rows: self.base_rows,
            extents,
            total_rows: self.total_rows,
        })
    }

    /// How many rows the base permutation covers — the boundary below which no flush and no merge
    /// moves a row, which is what makes an extents-only re-projection exact
    /// (`tessera_engine::compose::RowProjection::rebase_extents`).
    pub fn base_rows(&self) -> u32 {
        self.base_rows
    }

    /// Row ID currently occupied by `e`, or `None` if it has none — the base lookup below the
    /// build bound, otherwise a binary search over the extent list, `O(log k)`.
    pub fn row_of(&self, e: EntityId) -> Option<RowId> {
        if e.raw() < self.base.bound() {
            return self.base.row_of(e);
        }
        let raw = e.raw();
        let i = self
            .extents
            .partition_point(|extent| extent.entity_lo <= raw)
            .checked_sub(1)?;
        self.extents[i].row_of(raw)
    }

    /// Project an entity-space bitmap into this view's row space: the base projection unioned
    /// with each extent's own. The parts are disjoint — an extent's rows lie at or above
    /// `row_base`, which is where every earlier part ended — so the union is exact rather than
    /// merely a superset, and that is what lets a flush patch a cached projection instead of
    /// rebuilding it.
    ///
    /// Inherits [`Permutation::project`]'s cost note in full: never call this on a per-viewport
    /// path.
    pub fn project(&self, mask: &croaring::Bitmap) -> croaring::Bitmap {
        let mut rows = self.base.project(mask);
        rows.or_inplace(&self.project_extents_from(mask, 0));
        rows
    }

    /// Project into the **base** rows alone, ignoring every extent above them.
    ///
    /// **What the fold writes, and what a *generating set* is projected through**
    /// (`annotation-write-cycle.md` §4.1). A durable derived structure — a row-major column, a tile
    /// index's extents — is written over the rows the fold folded and describes nothing above them,
    /// so this is the projection that produces one and the projection a reader must compare it
    /// against.
    ///
    /// **Not the artifact row forms' projection any more.** A form covers the whole row space and
    /// is extended by each flush in place (`tessera_engine::artifacts`); what base-only bought was
    /// that a form survived a flush and a merge untouched, and what it cost was that a member
    /// ingested since the last fold contributed nothing to its artifact's masked count — fail-closed
    /// and hours wide under the nightly compaction gate. A generating set stays here, because the
    /// containment partition composed beside it is a function of the level's records and knows
    /// nothing of the geometry.
    pub fn project_base(&self, mask: &croaring::Bitmap) -> croaring::Bitmap {
        self.base.project(mask)
    }

    /// [`Self::project_base`], reusing a caller's scratch — see [`Permutation::project_with`].
    pub fn project_base_with(
        &self,
        mask: &croaring::Bitmap,
        scratch: &mut ProjectScratch,
    ) -> croaring::Bitmap {
        self.base.project_with(mask, scratch)
    }

    /// The rows contributed by the extents at or after `from` — the only part a flush recomputes.
    pub fn project_extents_from(&self, mask: &croaring::Bitmap, from: usize) -> croaring::Bitmap {
        let mut rows = croaring::Bitmap::new();
        for extent in &self.extents[from.min(self.extents.len())..] {
            rows.or_inplace(&extent.project(mask));
        }
        rows
    }

    /// The rows contributed by the one extent at `index` — what a merge's publication re-projects
    /// into a held row form for the span it renumbered (`tessera_engine::artifacts`). Empty where
    /// `index` names no extent.
    pub fn project_extent(&self, mask: &croaring::Bitmap, index: usize) -> croaring::Bitmap {
        match self.extents.get(index) {
            Some(extent) => extent.project(mask),
            None => croaring::Bitmap::new(),
        }
    }

    pub fn extent_count(&self) -> usize {
        self.extents.len()
    }

    pub fn extents(&self) -> &[SegmentExtent] {
        &self.extents
    }

    /// Every row this view holds, across the base and every extent.
    pub fn total_rows(&self) -> u64 {
        self.total_rows
    }

    /// The base permutation, for the callers that legitimately need the built segment alone —
    /// `tessera build`'s own verification, and the sharing assertion incremental generation
    /// construction rests on.
    pub fn base(&self) -> &Arc<Permutation> {
        &self.base
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use rand::rngs::StdRng;
    use rand::seq::SliceRandom;
    use rand::{Rng, SeedableRng};
    use tessera_types::EntityId;

    /// A permutation over `bound` entity slots in which each entity holds a row with probability
    /// `density`, in an order the seed decides — written through the real writer and read back
    /// through the real mmap path, so the slots the walk reads are the slots a bundle holds.
    fn fixture(dir: &Path, bound: u64, density: f64, rng: &mut StdRng) -> Permutation {
        let mut entities: Vec<EntityId> = (0..bound)
            .filter(|_| rng.gen_bool(density))
            .map(EntityId::new)
            .collect();
        entities.shuffle(rng);
        let path = dir.join("permutation.bin");
        crate::write::write_permutation(&path, &entities, bound).expect("write_permutation");
        Permutation::load(&path).expect("load permutation")
    }

    /// `count` entities drawn from `[0, bound)`, plus a handful above it — which a real mask holds
    /// whenever a flush has issued ids the base does not cover, and which the walk skips.
    fn mask_of(bound: u64, count: usize, rng: &mut StdRng) -> croaring::Bitmap {
        let mut mask = croaring::Bitmap::new();
        for _ in 0..count {
            mask.add(rng.gen_range(0..bound) as u32);
        }
        for above in 0..8u64 {
            mask.add((bound + above).min(u64::from(u32::MAX)) as u32);
        }
        mask
    }

    /// Windowing changes when containers are emitted, not which rows reach them. Several seeds
    /// rather than one fixture, because the property is over permutations and masks and a single
    /// pair of them checks one point of that space.
    #[test]
    fn a_windowed_pass_projects_what_a_single_pass_projects() {
        // Two buckets, the second a sliver, so the union runs across a bucket boundary; 5% of the
        // entities hold no row, so the walk skips sentinels as well as the ids above `bound` the
        // mask carries.
        const BOUND: u64 = (1 << 22) + 600_000;
        for seed in 0..3u64 {
            let dir = tempfile::tempdir().expect("tempdir");
            let mut rng = StdRng::seed_from_u64(seed);
            let perm = fixture(dir.path(), BOUND, 0.95, &mut rng);
            for count in [5_000usize, 1_000_000] {
                let mask = mask_of(BOUND, count, &mut rng);
                let mut scratch = ProjectScratch::default();
                let single = perm.project_windowed(&mask, &mut scratch, usize::MAX);
                for window in [1usize, 997, 100_000] {
                    let windowed = perm.project_windowed(&mask, &mut scratch, window);
                    assert_eq!(
                        windowed.iter().collect::<Vec<u32>>(),
                        single.iter().collect::<Vec<u32>>(),
                        "seed {seed}, {count} entities, window {window}: a windowed pass must \
                         project exactly what a single pass projects"
                    );
                }
            }
        }
    }

    /// The buckets hold one window and not the whole result, which is the bound the windowing
    /// exists for. Measured on the buckets themselves rather than on process memory: a window's
    /// worth of `u32`s plus the decode block that overshot it, and nothing proportional to the mask.
    #[test]
    fn the_buckets_never_hold_more_than_a_window() {
        const BOUND: u64 = (1 << 22) + 100_000;
        const WINDOW: usize = 10_000;
        let dir = tempfile::tempdir().expect("tempdir");
        let mut rng = StdRng::seed_from_u64(7);
        let perm = fixture(dir.path(), BOUND, 1.0, &mut rng);
        let mask = croaring::Bitmap::from_range(0..BOUND as u32);
        let mut scratch = ProjectScratch::default();
        let rows = perm.project_windowed(&mask, &mut scratch, WINDOW);
        assert_eq!(rows.cardinality(), BOUND, "every entity holds a row here");
        let held: usize = scratch.buckets.iter().map(|b| b.capacity()).sum();
        // A window plus the decode block that overshot it, doubled because one bucket can take the
        // whole window and a `Vec` that outgrows its reservation doubles. What the ceiling is a
        // function of is the window; what it is not a function of is the 4.29 million rows the mask
        // projected to, which is the whole of the property.
        let ceiling = 2 * (WINDOW + DECODE_WINDOW) + 64 * scratch.buckets.len();
        assert!(
            held <= ceiling,
            "the buckets kept {held} slots against a ceiling of {ceiling} for a {WINDOW}-row \
             window over {BOUND} rows"
        );
    }

    /// A mask over the whole of `[0, bound)` is answered as the row range, and that answer is the
    /// one the walk produces from the same mask.
    #[test]
    fn a_whole_domain_mask_is_the_row_range() {
        const BOUND: u64 = 200_000;
        let dir = tempfile::tempdir().expect("tempdir");
        let mut rng = StdRng::seed_from_u64(11);
        let perm = fixture(dir.path(), BOUND, 0.6, &mut rng);
        let rows = u32::try_from(
            (0..BOUND)
                .filter(|&e| perm.row_of(EntityId::new(e)).is_some())
                .count(),
        )
        .expect("fixture rows fit u32");
        perm.validate_rows(rows).expect("the fixture is a bijection");
        assert_eq!(perm.dense_rows.get(), Some(&rows));

        let mask = croaring::Bitmap::from_range(0..BOUND as u32);
        let mut scratch = ProjectScratch::default();
        let walked = perm.project_windowed(&mask, &mut scratch, usize::MAX);
        let answered = perm.project(&mask);
        assert_eq!(
            answered.iter().collect::<Vec<u32>>(),
            walked.iter().collect::<Vec<u32>>(),
            "the whole-domain answer must be the set the walk produces"
        );
        assert_eq!(answered, croaring::Bitmap::from_range(0..rows));

        // One entity short of the whole domain is not the whole domain, and takes the walk.
        let mut short = mask.clone();
        short.remove(rng.gen_range(0..BOUND) as u32);
        assert!(!perm.covers_domain(&short));
        assert_eq!(
            perm.project(&short).iter().collect::<Vec<u32>>(),
            perm.project_windowed(&short, &mut scratch, usize::MAX)
                .iter()
                .collect::<Vec<u32>>()
        );
    }

    /// A mapping that claims fewer rows than the descriptor declares is injective but not onto, so
    /// its image is not a range and the whole-domain answer must not be offered for it.
    #[test]
    fn a_mapping_that_claims_fewer_rows_than_declared_takes_the_walk() {
        const BOUND: u64 = 50_000;
        let dir = tempfile::tempdir().expect("tempdir");
        let mut rng = StdRng::seed_from_u64(13);
        let perm = fixture(dir.path(), BOUND, 0.5, &mut rng);
        let claimed = u32::try_from(
            (0..BOUND)
                .filter(|&e| perm.row_of(EntityId::new(e)).is_some())
                .count(),
        )
        .expect("fixture rows fit u32");
        perm.validate_rows(claimed + 100)
            .expect("every slot is still in bound and distinct");
        assert_eq!(perm.dense_rows.get(), None);

        let mask = croaring::Bitmap::from_range(0..BOUND as u32);
        let mut scratch = ProjectScratch::default();
        assert_eq!(
            perm.project(&mask),
            perm.project_windowed(&mask, &mut scratch, usize::MAX)
        );
    }
}
