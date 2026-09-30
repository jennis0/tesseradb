//! Writing sorted entries into run files.

use std::fs::{File, OpenOptions};
use std::io::{BufWriter, Seek, SeekFrom, Write};
use std::num::NonZeroU64;
use std::path::{Path, PathBuf};

use super::page::PageBuilder;
use super::{Header, Key, PAGE_SIZE};
use crate::error::{Result, StoreError};

/// One run file a writer finished.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WrittenRun<K: Key> {
    pub path: PathBuf,
    pub entries: u64,
    pub min_key: K,
    pub max_key: K,
}

/// Writes entries that arrive in strictly ascending `(key, entity)` order into run files named
/// `<stem>-<n>.keys` under a directory, `n` counting from 0.
///
/// A run is closed at the first key change once it holds `max_entries`, so a run exceeds that only
/// by the entities of its last key, and the runs one writer emits have disjoint key ranges. A
/// writer given no entries writes no file.
///
/// A file already at a run's name is an error, never overwritten. A writer dropped before
/// [`KeyRunWriter::finish`] returns removes every file it created.
pub struct KeyRunWriter<K: Key> {
    dir: PathBuf,
    stem: String,
    max_entries: u64,
    open: Option<OpenRun<K>>,
    written: Vec<WrittenRun<K>>,
    last: Option<(K, u32)>,
}

impl<K: Key> KeyRunWriter<K> {
    /// A writer into `dir`, which must exist.
    pub fn create(dir: &Path, stem: &str, max_entries: NonZeroU64) -> Self {
        KeyRunWriter {
            dir: dir.to_path_buf(),
            stem: stem.to_string(),
            max_entries: max_entries.get(),
            open: None,
            written: Vec::new(),
            last: None,
        }
    }

    /// Append one entry. It must follow the previous entry in `(key, entity)` order.
    pub fn push(&mut self, key: K, entity: u32) -> Result<()> {
        if let Some(last) = self.last {
            if (key, entity) <= last {
                return Err(StoreError::MalformedBundle {
                    detail: format!(
                        "key run {}: entries must arrive in strictly ascending (key, entity) \
                         order; sort them first, or write them through a KeySpill",
                        self.stem
                    ),
                });
            }
        }
        let key_changed = self.last.is_none_or(|(k, _)| k != key);
        if key_changed
            && self
                .open
                .as_ref()
                .is_some_and(|run| run.header.entries >= self.max_entries)
        {
            self.close()?;
        }
        if self.open.is_none() {
            let path = self
                .dir
                .join(format!("{}-{}.keys", self.stem, self.written.len()));
            self.open = Some(OpenRun::create(path)?);
        }
        self.open
            .as_mut()
            .expect("opened above")
            .push(key, entity)?;
        self.last = Some((key, entity));
        Ok(())
    }

    /// Close the last run and hand back every run written, in key order.
    pub fn finish(mut self) -> Result<Vec<WrittenRun<K>>> {
        self.close()?;
        Ok(std::mem::take(&mut self.written))
    }

    /// Finish the open run, if there is one; a run that fails to finish is removed.
    fn close(&mut self) -> Result<()> {
        if let Some(run) = self.open.take() {
            let path = run.path.clone();
            match run.finish() {
                Ok(written) => self.written.push(written),
                Err(e) => {
                    let _ = std::fs::remove_file(path);
                    return Err(e);
                }
            }
        }
        Ok(())
    }
}

impl<K: Key> Drop for KeyRunWriter<K> {
    fn drop(&mut self) {
        let open = self.open.take().map(|run| run.path);
        for path in self.written.iter().map(|w| &w.path).chain(open.as_ref()) {
            let _ = std::fs::remove_file(path);
        }
    }
}

/// The run file being written: pages go out as they fill; the header page is written last, over
/// the zeros that held its place.
struct OpenRun<K: Key> {
    path: PathBuf,
    out: BufWriter<File>,
    page: Vec<u8>,
    filling: PageBuilder<K>,
    first_keys: Vec<K>,
    header: Header,
}

impl<K: Key> OpenRun<K> {
    fn create(path: PathBuf) -> Result<Self> {
        let io = |source| StoreError::Io {
            path: path.clone(),
            source,
        };
        let file = OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&path)
            .map_err(io)?;
        let mut out = BufWriter::with_capacity(64 * PAGE_SIZE, file);
        out.write_all(&[0u8; PAGE_SIZE]).map_err(io)?;
        Ok(OpenRun {
            path: path.clone(),
            out,
            page: vec![0u8; PAGE_SIZE],
            filling: PageBuilder::new(),
            first_keys: Vec::new(),
            header: Header {
                key_width: K::WIDTH as u32,
                entries: 0,
                pages: 0,
                min_key: 0,
                max_key: 0,
            },
        })
    }

    fn io(&self, source: std::io::Error) -> StoreError {
        StoreError::Io {
            path: self.path.clone(),
            source,
        }
    }

    fn push(&mut self, key: K, entity: u32) -> Result<()> {
        if self.header.entries == 0 {
            self.header.min_key = key.widen();
        }
        self.header.max_key = key.widen();
        self.header.entries += 1;
        if !self.filling.try_push(key, entity) {
            self.seal_page()?;
            assert!(
                self.filling.try_push(key, entity),
                "an empty page takes any entry"
            );
        }
        Ok(())
    }

    fn seal_page(&mut self) -> Result<()> {
        self.first_keys.push(
            self.filling
                .first_key()
                .expect("a page is sealed with entries"),
        );
        self.filling.seal(&mut self.page);
        self.out.write_all(&self.page).map_err(|e| self.io(e))?;
        self.header.pages += 1;
        Ok(())
    }

    fn finish(mut self) -> Result<WrittenRun<K>> {
        if !self.filling.is_empty() {
            self.seal_page()?;
        }
        let mut index = vec![0u8; self.first_keys.len() * K::WIDTH];
        for (i, key) in self.first_keys.iter().enumerate() {
            key.write(&mut index[i * K::WIDTH..]);
        }
        let crc = crc32fast::hash(&index);
        self.out.write_all(&index).map_err(|e| self.io(e))?;
        self.out
            .write_all(&crc.to_le_bytes())
            .map_err(|e| self.io(e))?;
        let header = self.header.encode();
        let path = self.path.clone();
        let io = |source| StoreError::Io {
            path: path.clone(),
            source,
        };
        let mut file = self.out.into_inner().map_err(|e| io(e.into_error()))?;
        file.seek(SeekFrom::Start(0)).map_err(io)?;
        file.write_all(&header).map_err(io)?;
        Ok(WrittenRun {
            path: self.path,
            entries: self.header.entries,
            min_key: K::narrow(self.header.min_key),
            max_key: K::narrow(self.header.max_key),
        })
    }
}
