//! A file of `(key, value)` in ascending key order: what one field's holdings are kept in between
//! the files of a build.
//!
//! An integer key is written as its gap from the one before, in LEB128, and a keyword's hash whole;
//! the value follows in four bytes. The receipt carries the count and a mixed sum of what was
//! written, and the reader checks both at the end.

use std::fs::File;
use std::io::{BufReader, BufWriter, Read, Write};
use std::marker::PhantomData;
use std::path::{Path, PathBuf};

use tessera_store::key_index::Key;

use crate::error::{BuildError, Result};
use crate::spill::mix64;

const BUFFER: usize = 4 << 20;

/// What a run's reader is checked against.
#[derive(Debug, Clone)]
pub(super) struct RunReceipt {
    pub path: PathBuf,
    count: u64,
    anchor: u64,
}

fn entry_anchor(key: u128, value: u32) -> u64 {
    mix64(key as u64 ^ (key >> 64) as u64).wrapping_add(mix64(u64::from(value) << 1 | 1))
}

pub(super) struct RunWriter<K: Key> {
    path: PathBuf,
    writer: BufWriter<File>,
    last: u128,
    count: u64,
    anchor: u64,
    key: PhantomData<K>,
}

impl<K: Key> RunWriter<K> {
    pub fn create(path: &Path) -> Result<Self> {
        let file = File::create(path).map_err(|e| BuildError::io(path, e))?;
        Ok(RunWriter {
            path: path.to_path_buf(),
            writer: BufWriter::with_capacity(BUFFER, file),
            last: 0,
            count: 0,
            anchor: 0,
            key: PhantomData,
        })
    }

    /// The next entry; its key is at or above the last one's.
    pub fn push(&mut self, key: K, value: u32) -> Result<()> {
        let wide = key.widen();
        debug_assert!(wide >= self.last, "a run's keys ascend");
        let mut bytes = [0u8; 24];
        let mut at = 0;
        if K::WIDTH <= 8 {
            let mut gap = (wide - self.last) as u64;
            loop {
                let byte = (gap & 0x7f) as u8;
                gap >>= 7;
                if gap == 0 {
                    bytes[at] = byte;
                    at += 1;
                    break;
                }
                bytes[at] = byte | 0x80;
                at += 1;
            }
        } else {
            key.write(&mut bytes[..K::WIDTH]);
            at = K::WIDTH;
        }
        bytes[at..at + 4].copy_from_slice(&value.to_le_bytes());
        self.writer
            .write_all(&bytes[..at + 4])
            .map_err(|e| BuildError::io(&self.path, e))?;
        self.last = wide;
        self.count += 1;
        self.anchor = self.anchor.wrapping_add(entry_anchor(wide, value));
        Ok(())
    }

    pub fn finish(self) -> Result<RunReceipt> {
        let RunWriter {
            path,
            writer,
            count,
            anchor,
            ..
        } = self;
        writer
            .into_inner()
            .map_err(|e| BuildError::io(&path, e.into_error()))?;
        Ok(RunReceipt {
            path,
            count,
            anchor,
        })
    }
}

pub(super) struct RunReader<K: Key> {
    receipt: RunReceipt,
    reader: BufReader<File>,
    last: u128,
    left: u64,
    anchor: u64,
    key: PhantomData<K>,
}

impl<K: Key> RunReader<K> {
    pub fn open(receipt: &RunReceipt) -> Result<Self> {
        let file = File::open(&receipt.path).map_err(|e| BuildError::io(&receipt.path, e))?;
        Ok(RunReader {
            receipt: receipt.clone(),
            reader: BufReader::with_capacity(BUFFER, file),
            last: 0,
            left: receipt.count,
            anchor: 0,
            key: PhantomData,
        })
    }

    fn byte(&mut self) -> Result<u8> {
        let mut byte = [0u8; 1];
        self.reader
            .read_exact(&mut byte)
            .map_err(|e| BuildError::io(&self.receipt.path, e))?;
        Ok(byte[0])
    }

    /// The next entry, or `None` past the last, which is where the run is checked.
    pub fn next_entry(&mut self) -> Result<Option<(K, u32)>> {
        if self.left == 0 {
            let mut rest = [0u8; 1];
            let trailing = self
                .reader
                .read(&mut rest)
                .map_err(|e| BuildError::io(&self.receipt.path, e))?;
            if trailing != 0 || self.anchor != self.receipt.anchor {
                return Err(BuildError::Invalid(format!(
                    "{} is not the run this build wrote: its entries do not add up to its \
                     receipt. Build again",
                    self.receipt.path.display()
                )));
            }
            return Ok(None);
        }
        let wide = if K::WIDTH <= 8 {
            let mut gap = 0u64;
            let mut shift = 0;
            loop {
                let byte = self.byte()?;
                gap |= u64::from(byte & 0x7f) << shift;
                if byte & 0x80 == 0 {
                    break;
                }
                shift += 7;
            }
            self.last + u128::from(gap)
        } else {
            let mut bytes = [0u8; 16];
            self.reader
                .read_exact(&mut bytes[..K::WIDTH])
                .map_err(|e| BuildError::io(&self.receipt.path, e))?;
            K::read(&bytes[..K::WIDTH]).widen()
        };
        let mut value = [0u8; 4];
        self.reader
            .read_exact(&mut value)
            .map_err(|e| BuildError::io(&self.receipt.path, e))?;
        let value = u32::from_le_bytes(value);
        self.last = wide;
        self.left -= 1;
        self.anchor = self.anchor.wrapping_add(entry_anchor(wide, value));
        Ok(Some((K::narrow(wide), value)))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_run_reads_back_what_was_written_at_either_width() {
        let dir = tempfile::tempdir().unwrap();
        let ints: Vec<(u64, u32)> = vec![(0, 7), (0, 8), (5, 1), (1 << 40, 2), (u64::MAX, 3)];
        let path = dir.path().join("ints");
        let mut writer = RunWriter::<u64>::create(&path).unwrap();
        for &(key, value) in &ints {
            writer.push(key, value).unwrap();
        }
        let receipt = writer.finish().unwrap();
        let mut reader = RunReader::<u64>::open(&receipt).unwrap();
        let mut back = Vec::new();
        while let Some(entry) = reader.next_entry().unwrap() {
            back.push(entry);
        }
        assert_eq!(back, ints);

        let words: Vec<(u128, u32)> = vec![(3, 1), (u128::MAX - 1, 2)];
        let path = dir.path().join("words");
        let mut writer = RunWriter::<u128>::create(&path).unwrap();
        for &(key, value) in &words {
            writer.push(key, value).unwrap();
        }
        let receipt = writer.finish().unwrap();
        let mut reader = RunReader::<u128>::open(&receipt).unwrap();
        let mut back = Vec::new();
        while let Some(entry) = reader.next_entry().unwrap() {
            back.push(entry);
        }
        assert_eq!(back, words);
    }

    #[test]
    fn a_run_whose_bytes_changed_is_refused_at_its_end() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("run");
        let mut writer = RunWriter::<u64>::create(&path).unwrap();
        writer.push(10, 1).unwrap();
        writer.push(20, 2).unwrap();
        let receipt = writer.finish().unwrap();
        let mut bytes = std::fs::read(&path).unwrap();
        let last = bytes.len() - 1;
        bytes[last] ^= 1;
        std::fs::write(&path, bytes).unwrap();
        let mut reader = RunReader::<u64>::open(&receipt).unwrap();
        let refused = loop {
            match reader.next_entry() {
                Ok(Some(_)) => continue,
                Ok(None) => break false,
                Err(_) => break true,
            }
        };
        assert!(refused);
    }
}
