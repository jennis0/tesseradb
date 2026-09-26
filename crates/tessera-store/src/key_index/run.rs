//! Reading one run: open, point and batched lookups, a full scan, and the whole-file check.

use std::fs::File;
use std::marker::PhantomData;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};

use memmap2::{Advice, Mmap};

use super::{entries_per_page, file_len, Header, Key, RunPart, HEADER_LEN, PAGE_BODY, PAGE_SIZE};
use crate::error::{Result, StoreError};

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

/// One checked entry page.
struct Page<'a, K: Key> {
    bytes: &'a [u8],
    len: usize,
    key: PhantomData<K>,
}

impl<K: Key> Page<'_, K> {
    const ENTRY: usize = K::WIDTH + 4;

    #[inline]
    fn key(&self, i: usize) -> K {
        K::read(&self.bytes[i * Self::ENTRY..])
    }

    #[inline]
    fn entity(&self, i: usize) -> u32 {
        let at = i * Self::ENTRY + K::WIDTH;
        u32::from_le_bytes(self.bytes[at..at + 4].try_into().expect("four bytes"))
    }

    /// The first entry whose key is at or past `key`.
    fn lower_bound(&self, key: K) -> usize {
        let (mut lo, mut hi) = (0, self.len);
        while lo < hi {
            let mid = lo + (hi - lo) / 2;
            if self.key(mid) < key {
                lo = mid + 1;
            } else {
                hi = mid;
            }
        }
        lo
    }
}

impl<K: Key> KeyRun<K> {
    /// Open the run at `path` for lookups: its entry pages are read at random.
    pub fn open(path: &Path) -> Result<Self> {
        Self::open_with(path, Advice::Random)
    }

    /// Open the run at `path` to be read front to back, as a merge does.
    pub(super) fn open_sequential(path: &Path) -> Result<Self> {
        Self::open_with(path, Advice::Sequential)
    }

    fn open_with(path: &Path, advice: Advice) -> Result<Self> {
        let io = |source| StoreError::Io {
            path: path.to_path_buf(),
            source,
        };
        let file = File::open(path).map_err(io)?;
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
        let per_page = entries_per_page(width) as u64;
        if header.pages != header.entries.div_ceil(per_page) {
            return Err(header_err(format!(
                "{} entries need {} pages and the header says {}",
                header.entries,
                header.entries.div_ceil(per_page),
                header.pages
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

    /// Entry page `p`, checked if this is its first read.
    fn page(&self, p: u64) -> Result<Page<'_, K>> {
        let at = (1 + p as usize) * PAGE_SIZE;
        let bytes = &self.map[at..at + PAGE_SIZE];
        let per_page = entries_per_page(K::WIDTH) as u64;
        let len = (self.header.entries - p * per_page).min(per_page) as usize;
        let page = Page {
            bytes,
            len,
            key: PhantomData,
        };
        if !self.is_verified(p) {
            self.check_page(p, &page)?;
            self.verified[(p / 64) as usize].fetch_or(1 << (p % 64), Ordering::Release);
        }
        Ok(page)
    }

    /// A page's checksum, the order of its entries, and its first and last keys against the page
    /// index and the header.
    fn check_page(&self, p: u64, page: &Page<'_, K>) -> Result<()> {
        let fail = |detail: &str| corrupt(&self.path, RunPart::Page(p), detail);
        let stored = u32::from_le_bytes(page.bytes[PAGE_BODY..].try_into().expect("four bytes"));
        if crc32fast::hash(&page.bytes[..PAGE_BODY]) != stored {
            return Err(fail("the page checksum does not match its bytes"));
        }
        let entry = |i: usize| (page.key(i), page.entity(i));
        if (1..page.len).any(|i| entry(i - 1) >= entry(i)) {
            return Err(fail(
                "the page's entries are not in ascending (key, entity) order",
            ));
        }
        if page.key(0) != self.index_key(p) {
            return Err(fail("the page's first key disagrees with the page index"));
        }
        let last = page.key(page.len - 1);
        if p + 1 < self.header.pages {
            if last > self.index_key(p + 1) {
                return Err(fail("the page's last key is past the next page's first"));
            }
        } else if last.widen() != self.header.max_key {
            return Err(fail(
                "the last page's last key is not the header's largest key",
            ));
        }
        Ok(())
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
            self.scan(key, 0, |entity| out.push(entity))?;
        }
        Ok(out)
    }

    /// Every entity stored under each of `keys`, as `(position in keys, entity)` in the order of
    /// `keys`. `keys` must be in ascending order (repeats allowed); the run is walked once, front
    /// to back.
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
        let mut out = Vec::new();
        let mut from = 0;
        for (i, &key) in keys.iter().enumerate() {
            if self.may_hold(key) {
                from = self.scan(key, from, |entity| out.push((i, entity)))?;
            }
        }
        Ok(out)
    }

    /// Ask the kernel to read, in the background, the first page each of `keys` will read that
    /// has not been read before. The entry pages are mapped for random access, so without this
    /// a batch reads its pages one at a time, each waiting on the disk.
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
            if self.is_verified(from) {
                continue;
            }
            pending = match pending {
                Some((first, end)) if from <= end => Some((first, end.max(from + 1))),
                other => {
                    if let Some(range) = other {
                        advise(range);
                    }
                    Some((from, from + 1))
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

    /// Hand every entity under `key` to `found`, reading pages from `from` on, and return the page
    /// the scan started at. No entry of `key` is before `from` when `from` is where the scan for
    /// a key at or below `key` started.
    fn scan(&self, key: K, from: u64, mut found: impl FnMut(u32)) -> Result<u64> {
        let start = self.start_page(key, from);
        for p in start..self.header.pages {
            if self.index_key(p) > key {
                break;
            }
            let page = self.page(p)?;
            let mut i = page.lower_bound(key);
            while i < page.len && page.key(i) == key {
                found(page.entity(i));
                i += 1;
            }
            if i < page.len {
                break;
            }
        }
        Ok(start)
    }

    /// Every entry in order, each page checked as it is reached.
    pub fn iter(&self) -> Entries<'_, K> {
        Entries {
            run: self,
            page: None,
            next_page: 0,
            at: 0,
        }
    }
}

/// The entries of a run in `(key, entity)` order; see [`KeyRun::iter`]. A page that fails its
/// checks ends the iteration with that error.
pub struct Entries<'a, K: Key> {
    run: &'a KeyRun<K>,
    page: Option<Page<'a, K>>,
    next_page: u64,
    at: usize,
}

impl<K: Key> Iterator for Entries<'_, K> {
    type Item = Result<(K, u32)>;

    fn next(&mut self) -> Option<Self::Item> {
        loop {
            if let Some(page) = &self.page {
                if self.at < page.len {
                    let entry = (page.key(self.at), page.entity(self.at));
                    self.at += 1;
                    return Some(Ok(entry));
                }
            }
            if self.next_page >= self.run.header.pages {
                return None;
            }
            match self.run.page(self.next_page) {
                Ok(page) => {
                    self.page = Some(page);
                    self.next_page += 1;
                    self.at = 0;
                }
                Err(e) => {
                    self.next_page = self.run.header.pages;
                    self.page = None;
                    return Some(Err(e));
                }
            }
        }
    }
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
    let mut first = vec![0u8; PAGE_SIZE];
    {
        use std::io::Read;
        let mut file = File::open(path).map_err(io)?;
        let read = file.read(&mut first).map_err(io)?;
        first.truncate(read);
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
    let mut previous: Option<(K, u32)> = None;
    for p in 0..run.header.pages {
        let page = run.page(p)?;
        let fail = |detail: &str| corrupt(path, RunPart::Page(p), detail);
        let used = page.len * (K::WIDTH + 4);
        if page.bytes[used..PAGE_BODY].iter().any(|&b| b != 0) {
            return Err(fail("the page's bytes after its last entry are not zero"));
        }
        let first = (page.key(0), page.entity(0));
        if previous.is_some_and(|prev| prev >= first) {
            return Err(fail(
                "the page's first entry does not follow the previous page's last",
            ));
        }
        previous = Some((page.key(page.len - 1), page.entity(page.len - 1)));
    }
    Ok(RunCheck {
        key_width: K::WIDTH,
        entries: run.header.entries,
        pages: run.header.pages,
    })
}
