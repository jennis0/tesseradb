//! Reading one run: open, point and batched lookups, a full scan, and the whole-file check.

use std::fs::File;
use std::marker::PhantomData;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};

use memmap2::{Advice, Mmap};

use super::page::{max_entries, Page, PageCursor};
use super::{file_len, Header, Key, RunPart, HEADER_LEN, PAGE_BODY, PAGE_SIZE};
use crate::error::{Result, StoreError};

/// The most pages between two ranges a batched lookup needs that it prefetches with them.
const PREFETCH_GAP: u64 = 4;

fn corrupt(path: &Path, part: RunPart, detail: impl Into<String>) -> StoreError {
    StoreError::InvalidKeyIndex {
        path: path.to_path_buf(),
        part,
        detail: detail.into(),
    }
}

/// One run file, mapped. Opening reads the header and the page index; each entry page is checked
/// the first time it is read (see the module doc of [`crate::key_index`]).
pub struct KeyRun<K: Key> {
    path: PathBuf,
    map: Mmap,
    header: Header,
    /// One bit per entry page, set once the page has passed its checks.
    verified: Box<[AtomicU64]>,
    key: PhantomData<K>,
}

impl<K: Key> KeyRun<K> {
    /// Open the run at `path` for lookups: its entry pages are read at random.
    pub fn open(path: &Path) -> Result<Self> {
        Self::open_with(path, Advice::Random)
    }

    /// Open the run at `path` to be read front to back, as a merge does.
    pub(crate) fn open_sequential(path: &Path) -> Result<Self> {
        Self::open_with(path, Advice::Sequential)
    }

    fn open_with(path: &Path, advice: Advice) -> Result<Self> {
        let io = |source| StoreError::Io {
            path: path.to_path_buf(),
            source,
        };
        let file = File::open(path).map_err(io)?;
        let len = file.metadata().map_err(io)?.len();
        if len < PAGE_SIZE as u64 {
            return Err(corrupt(
                path,
                RunPart::Header,
                format!("the file is {len} bytes, shorter than its {PAGE_SIZE}-byte header page"),
            ));
        }
        // SAFETY: a run file is written once and never modified or truncated after it is
        // published; the mapping is read-only.
        let map = unsafe { Mmap::map(&file) }.map_err(io)?;
        // Before the first read, so reading the header does not read ahead into the entries.
        let _ = map.advise(advice);
        let header =
            Header::decode(&map).map_err(|detail| corrupt(path, RunPart::Header, detail))?;
        let width = K::WIDTH;
        let header_err = |detail: String| corrupt(path, RunPart::Header, detail);
        if header.key_width as usize != width {
            return Err(header_err(format!(
                "the run holds {}-byte keys and was opened for {width}-byte keys; open it with \
                 the key type it was written with",
                header.key_width
            )));
        }
        let most = header.pages.saturating_mul(max_entries(width) as u64);
        if header.pages > header.entries || header.entries > most {
            return Err(header_err(format!(
                "{} entries do not fit {} pages; a page holds from one entry to {}",
                header.entries,
                header.pages,
                max_entries(width)
            )));
        }
        let key_limit = if width == 16 {
            u128::MAX
        } else {
            (1u128 << (8 * width)) - 1
        };
        if header.entries > 0 && (header.min_key > header.max_key || header.max_key > key_limit) {
            return Err(header_err(
                "the smallest and largest keys are out of order or wider than the key".to_string(),
            ));
        }
        let expected = file_len(header.pages, width)
            .ok_or_else(|| header_err("the page count overflows a file length".to_string()))?;
        if map.len() as u64 != expected {
            return Err(header_err(format!(
                "the file is {} bytes and a run of {} pages is {expected}; the file is truncated \
                 or extended",
                map.len(),
                header.pages
            )));
        }

        let words = header.pages.div_ceil(64) as usize;
        let run = KeyRun {
            path: path.to_path_buf(),
            map,
            header,
            verified: (0..words).map(|_| AtomicU64::new(0)).collect(),
            key: PhantomData,
        };
        let index_len = run.header.pages as usize * K::WIDTH + 4;
        let _ = run
            .map
            .advise_range(Advice::WillNeed, run.index_offset(), index_len);
        run.check_page_index()?;
        Ok(run)
    }

    /// The page index's checksum, its order, and its agreement with the header's key range.
    fn check_page_index(&self) -> Result<()> {
        let fail = |detail: &str| corrupt(&self.path, RunPart::PageIndex, detail);
        let start = self.index_offset();
        let end = start + self.header.pages as usize * K::WIDTH;
        let stored = u32::from_le_bytes(self.map[end..end + 4].try_into().expect("four bytes"));
        if crc32fast::hash(&self.map[start..end]) != stored {
            return Err(fail("the page index checksum does not match its bytes"));
        }
        let pages = self.header.pages;
        if pages == 0 {
            return Ok(());
        }
        if (1..pages).any(|p| self.index_key(p - 1) > self.index_key(p)) {
            return Err(fail(
                "the first keys of the pages are not in ascending order",
            ));
        }
        if self.index_key(0).widen() != self.header.min_key
            || self.index_key(pages - 1).widen() > self.header.max_key
        {
            return Err(fail("the page index disagrees with the header's key range"));
        }
        Ok(())
    }

    fn index_offset(&self) -> usize {
        (1 + self.header.pages as usize) * PAGE_SIZE
    }

    /// The first key of entry page `p`, from the page index.
    #[inline]
    fn index_key(&self, p: u64) -> K {
        K::read(&self.map[self.index_offset() + p as usize * K::WIDTH..])
    }

    /// Entry page `p`'s bytes, unchecked.
    fn page_bytes(&self, p: u64) -> &[u8] {
        let at = (1 + p as usize) * PAGE_SIZE;
        &self.map[at..at + PAGE_SIZE]
    }

    /// Entry page `p`, checked if this is its first read.
    fn page(&self, p: u64) -> Result<Page<'_, K>> {
        let bytes = self.page_bytes(p);
        if self.is_verified(p) {
            return Page::parse(bytes)
                .map_err(|detail| corrupt(&self.path, RunPart::Page(p), detail));
        }
        let page = self.check_page(p, bytes)?;
        self.verified[(p / 64) as usize].fetch_or(1 << (p % 64), Ordering::Release);
        Ok(page)
    }

    /// A page's checksum, its count and gap width, that the bits and bytes it leaves unused are
    /// zero, the order of its entries, its first and last
    /// keys against the page index and the header, and the order of its first and last entries
    /// against those of a neighbouring page already checked. Whichever of two neighbours is
    /// checked second compares the two, so every pair of neighbouring pages a lookup has read is
    /// in order.
    fn check_page<'a>(&'a self, p: u64, bytes: &'a [u8]) -> Result<Page<'a, K>> {
        let fail = |detail: &str| corrupt(&self.path, RunPart::Page(p), detail);
        let stored = u32::from_le_bytes(bytes[PAGE_BODY..].try_into().expect("four bytes"));
        if crc32fast::hash(&bytes[..PAGE_BODY]) != stored {
            return Err(fail("the page checksum does not match its bytes"));
        }
        let page = Page::<K>::parse(bytes).map_err(fail)?;
        if page.gap_padding() != 0 || bytes[page.used()..PAGE_BODY].iter().any(|&b| b != 0) {
            return Err(fail(
                "the page's bits after its last gap or bytes after its last entity are not zero",
            ));
        }
        let mut last: Option<(u128, u32)> = None;
        for (i, key) in page.checked_keys().enumerate() {
            let key =
                key.ok_or_else(|| fail("the page's gaps carry a key past the largest key"))?;
            let entry = (key, page.entity(i));
            if last.is_some_and(|last| last >= entry) {
                return Err(fail(
                    "the page's entries are not in ascending (key, entity) order",
                ));
            }
            last = Some(entry);
        }
        let (last_key, last_entity) = last.expect("a parsed page has an entry");
        let last_key = K::narrow(last_key);
        if page.first_key() != self.index_key(p) {
            return Err(fail("the page's first key disagrees with the page index"));
        }
        if p + 1 < self.header.pages {
            if last_key > self.index_key(p + 1) {
                return Err(fail("the page's last key is past the next page's first"));
            }
        } else if last_key.widen() != self.header.max_key {
            return Err(fail(
                "the last page's last key is not the header's largest key",
            ));
        }
        let neighbour =
            |q: u64| Page::<K>::parse(self.page_bytes(q)).expect("a checked page parses");
        if p + 1 < self.header.pages && self.is_verified(p + 1) {
            let next = neighbour(p + 1);
            if (last_key, last_entity) >= (next.first_key(), next.entity(0)) {
                return Err(fail(
                    "the page's last entry does not precede the next page's first",
                ));
            }
        }
        if p > 0
            && self.is_verified(p - 1)
            && neighbour(p - 1).last() >= (page.first_key(), page.entity(0))
        {
            return Err(fail(
                "the page's first entry does not follow the previous page's last",
            ));
        }
        Ok(page)
    }

    pub fn path(&self) -> &Path {
        &self.path
    }

    /// Entries in the run.
    pub fn len(&self) -> u64 {
        self.header.entries
    }

    pub fn is_empty(&self) -> bool {
        self.header.entries == 0
    }

    /// The smallest and largest key in the run, from the header; `None` for an empty run.
    pub fn key_range(&self) -> Option<(K, K)> {
        (self.header.entries > 0).then(|| {
            (
                K::narrow(self.header.min_key),
                K::narrow(self.header.max_key),
            )
        })
    }

    fn may_hold(&self, key: K) -> bool {
        self.key_range()
            .is_some_and(|(lo, hi)| lo <= key && key <= hi)
    }

    /// Every entity stored under `key`, ascending.
    pub fn get(&self, key: K) -> Result<Vec<u32>> {
        let mut out = Vec::new();
        if self.may_hold(key) {
            self.scan(key, &mut None, |entity| out.push(entity))?;
        }
        Ok(out)
    }

    /// Every entity stored under each of `keys`, as `(position in keys, entity)` in the order of
    /// `keys`. `keys` must be in ascending order (repeats allowed); the run is walked once, front
    /// to back, and each page it reads is decoded once.
    ///
    /// # Panics
    ///
    /// If `keys` is not in ascending order.
    pub fn lookup_sorted(&self, keys: &[K]) -> Result<Vec<(usize, u32)>> {
        assert!(
            keys.windows(2).all(|w| w[0] <= w[1]),
            "lookup_sorted takes keys in ascending order"
        );
        self.prefetch(keys);
        let mut out: Vec<(usize, u32)> = Vec::new();
        let mut at = None;
        // Where the previous key's entities start in `out`, for a repeated key.
        let mut previous: Option<(K, usize)> = None;
        for (i, &key) in keys.iter().enumerate() {
            if !self.may_hold(key) {
                continue;
            }
            match previous {
                Some((k, from)) if k == key => {
                    let end = out.len();
                    out.extend_from_within(from..end);
                    let copied = end - from;
                    for entry in &mut out[end..end + copied] {
                        entry.0 = i;
                    }
                    previous = Some((key, end));
                }
                _ => {
                    let from = out.len();
                    self.scan(key, &mut at, |entity| out.push((i, entity)))?;
                    previous = Some((key, from));
                }
            }
        }
        Ok(out)
    }

    /// Ask the kernel to read, in the background, every page a lookup of `keys` will read. The
    /// entry pages are mapped for random access, so without this a batch reads its pages one at
    /// a time, each waiting on the disk.
    ///
    /// Pages are advised in ranges, and a gap of up to [`PREFETCH_GAP`] pages between two ranges
    /// is advised with them: reading a few unneeded pages costs less than a system call per range
    /// when the pages are already resident.
    fn prefetch(&self, keys: &[K]) {
        let advise = |(first, end): (u64, u64)| {
            let _ = self.map.advise_range(
                Advice::WillNeed,
                (1 + first as usize) * PAGE_SIZE,
                (end - first) as usize * PAGE_SIZE,
            );
        };
        let mut pending: Option<(u64, u64)> = None;
        let mut from = 0;
        for &key in keys {
            if !self.may_hold(key) {
                continue;
            }
            from = self.start_page(key, from);
            let mut end = from + 1;
            while end < self.header.pages && self.index_key(end) <= key {
                end += 1;
            }
            pending = match pending {
                Some((first, last_end)) if from <= last_end + PREFETCH_GAP => {
                    Some((first, last_end.max(end)))
                }
                other => {
                    if let Some(range) = other {
                        advise(range);
                    }
                    Some((from, end))
                }
            };
        }
        if let Some(range) = pending {
            advise(range);
        }
    }

    fn is_verified(&self, p: u64) -> bool {
        self.verified[(p / 64) as usize].load(Ordering::Acquire) & (1 << (p % 64)) != 0
    }

    /// The page a scan for `key` starts at, searching from page `from` on: the page before the
    /// first whose first key is at or past `key`, since that page may hold `key` at its end.
    fn start_page(&self, key: K, from: u64) -> u64 {
        let (mut lo, mut hi) = (from, self.header.pages);
        while lo < hi {
            let mid = lo + (hi - lo) / 2;
            if self.index_key(mid) < key {
                lo = mid + 1;
            } else {
                hi = mid;
            }
        }
        if lo > from {
            lo - 1
        } else {
            lo
        }
    }

    /// Hand every entity under `key` to `found`, continuing from `at`, where a scan for a smaller
    /// key left it, or from the start when `at` is `None`; `at` is left past the last entity
    /// under `key`, or on the last entry of the page the scan ended in.
    fn scan<'a>(
        &'a self,
        key: K,
        at: &mut Option<(u64, PageCursor<'a, K>)>,
        mut found: impl FnMut(u32),
    ) -> Result<()> {
        let from = at.as_ref().map_or(0, |&(p, _)| p);
        let start = self.start_page(key, from);
        if at.as_ref().is_none_or(|&(p, _)| p != start) {
            *at = Some((start, PageCursor::new(self.page(start)?)));
        }
        let (p, cursor) = at.as_mut().expect("set above");
        loop {
            cursor.seek(key);
            loop {
                if cursor.key > key {
                    return Ok(());
                }
                if cursor.key == key {
                    found(cursor.entity());
                }
                if !cursor.step() {
                    break;
                }
            }
            let next = *p + 1;
            if next >= self.header.pages || self.index_key(next) > key {
                return Ok(());
            }
            *cursor = PageCursor::new(self.page(next)?);
            *p = next;
        }
    }

    /// Every entry in order, each page checked as it is reached.
    pub fn iter(&self) -> Entries<'_, K> {
        Entries {
            run: self,
            cursor: None,
            next_page: 0,
            seen: 0,
        }
    }
}

/// The entries of a run in `(key, entity)` order; see [`KeyRun::iter`]. A page that fails its
/// checks, or pages holding other than the header's entry count, end the iteration with that
/// error.
pub struct Entries<'a, K: Key> {
    run: &'a KeyRun<K>,
    cursor: Option<PageCursor<'a, K>>,
    next_page: u64,
    seen: u64,
}

impl<K: Key> Iterator for Entries<'_, K> {
    type Item = Result<(K, u32)>;

    fn next(&mut self) -> Option<Self::Item> {
        if let Some(cursor) = &mut self.cursor {
            if cursor.step() {
                self.seen += 1;
                return Some(Ok((cursor.key, cursor.entity())));
            }
        }
        let run = self.run;
        if self.next_page >= run.header.pages {
            if self.cursor.take().is_some() && self.seen != run.header.entries {
                return Some(Err(corrupt(
                    &run.path,
                    RunPart::Header,
                    format!(
                        "the pages hold {} entries and the header says {}",
                        self.seen, run.header.entries
                    ),
                )));
            }
            return None;
        }
        match run.page(self.next_page) {
            Ok(page) => {
                let cursor = PageCursor::new(page);
                self.next_page += 1;
                self.seen += 1;
                self.cursor = Some(cursor);
                Some(Ok((cursor.key, cursor.entity())))
            }
            Err(e) => {
                self.next_page = run.header.pages;
                self.cursor = None;
                Some(Err(e))
            }
        }
    }
}

/// The key width, in bytes, the run at `path` was written with, from its header page alone.
pub fn run_key_width(path: &Path) -> Result<usize> {
    let io = |source| StoreError::Io {
        path: path.to_path_buf(),
        source,
    };
    let mut first = Vec::with_capacity(PAGE_SIZE);
    {
        use std::io::Read;
        let file = File::open(path).map_err(io)?;
        file.take(PAGE_SIZE as u64)
            .read_to_end(&mut first)
            .map_err(io)?;
    }
    let header = Header::decode(&first).map_err(|detail| corrupt(path, RunPart::Header, detail))?;
    Ok(header.key_width as usize)
}

/// What [`verify_run`] found in a run that passed.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct RunCheck {
    pub key_width: usize,
    pub entries: u64,
    pub pages: u64,
}

/// Check the run at `path` completely: the header, the page index, every page's checksum, the
/// order of every entry within and across pages, the page index against every page, the header's
/// counts and key range, and that the bytes the format leaves unused are zero. Reads every byte.
pub fn verify_run(path: &Path) -> Result<RunCheck> {
    let io = |source| StoreError::Io {
        path: path.to_path_buf(),
        source,
    };
    let mut first = Vec::with_capacity(PAGE_SIZE);
    {
        use std::io::Read;
        let file = File::open(path).map_err(io)?;
        file.take(PAGE_SIZE as u64)
            .read_to_end(&mut first)
            .map_err(io)?;
    }
    let header = Header::decode(&first).map_err(|detail| corrupt(path, RunPart::Header, detail))?;
    match header.key_width {
        4 => verify_all::<u32>(path),
        8 => verify_all::<u64>(path),
        16 => verify_all::<u128>(path),
        w => Err(corrupt(
            path,
            RunPart::Header,
            format!("the key width is {w} bytes; a run's keys are 4, 8 or 16 bytes"),
        )),
    }
}

fn verify_all<K: Key>(path: &Path) -> Result<RunCheck> {
    let run = KeyRun::<K>::open_sequential(path)?;
    if run.map[HEADER_LEN + 4..PAGE_SIZE].iter().any(|&b| b != 0) {
        return Err(corrupt(
            path,
            RunPart::Header,
            "the header page's unused bytes are not zero",
        ));
    }
    // Reading the pages in order checks each against the one before it (see `check_page`).
    let mut entries = 0u64;
    for p in 0..run.header.pages {
        entries += run.page(p)?.len() as u64;
    }
    if entries != run.header.entries {
        return Err(corrupt(
            path,
            RunPart::Header,
            format!(
                "the pages hold {entries} entries and the header says {}",
                run.header.entries
            ),
        ));
    }
    Ok(RunCheck {
        key_width: K::WIDTH,
        entries: run.header.entries,
        pages: run.header.pages,
    })
}
