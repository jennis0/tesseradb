use rustc_hash::FxHashMap;
use std::fs::File;
use std::io::{self, BufWriter, Write};
use std::path::{Path, PathBuf};
use tessera_types::TermId;

/// Writes dictionary extents: deduplicates descriptors and assigns ordinal term IDs.
pub struct DictWriter {
    dir: PathBuf,
    interner: FxHashMap<Box<[u8]>, TermId>,
    next_term_id: u32,
}

impl DictWriter {
    /// Create a new dictionary writer for the given directory.
    pub fn new(dir: &Path) -> Self {
        DictWriter {
            dir: dir.to_path_buf(),
            interner: FxHashMap::default(),
            next_term_id: 0,
        }
    }

    /// Intern a descriptor, returning its term ID (deduplicating).
    pub fn intern(&mut self, descriptor: &[u8]) -> TermId {
        let key = descriptor.to_vec().into_boxed_slice();
        if let Some(&term_id) = self.interner.get(&key) {
            term_id
        } else {
            let term_id = TermId::new(self.next_term_id);
            self.next_term_id += 1;
            self.interner.insert(key, term_id);
            term_id
        }
    }

    /// The number of distinct descriptors interned so far — equivalently, the number of records
    /// `finish` will write, and the next term ID that would be assigned. Callers sizing a
    /// term-indexed array (the postings writer's `per_term`) must use this rather than
    /// `max(term_id) + 1` over the items they happen to hold: the two agree only when every
    /// interned term is still reachable from some item, which is an assumption, not a guarantee.
    pub fn len(&self) -> u32 {
        self.next_term_id
    }

    /// Whether nothing has been interned yet.
    pub fn is_empty(&self) -> bool {
        self.next_term_id == 0
    }

    /// Finish writing: persist the dictionary to `terms-0.dict` and return the paths written.
    pub fn finish(self) -> io::Result<Vec<PathBuf>> {
        let dict_path = self.dir.join("terms-0.dict");

        // Collect terms in ordinal order
        let mut terms: Vec<_> = self.interner.iter().collect();
        terms.sort_by_key(|(_, &term_id)| term_id);

        // Write the file: each record is u32 LE length ‖ descriptor bytes
        let mut data = Vec::new();
        for (descriptor, _) in terms {
            let len = descriptor.len() as u32;
            data.extend_from_slice(&len.to_le_bytes());
            data.extend_from_slice(descriptor);
        }

        std::fs::write(&dict_path, data)?;
        Ok(vec![dict_path])
    }
}

/// Streaming counterpart to [`DictWriter`]: each descriptor goes straight through a buffered
/// file handle instead of being interned, so memory stays O(1) in the descriptor count.
///
/// Caller contract: descriptors must be distinct and arrive in term-id order — `append` assigns
/// sequential ids from 0 and never deduplicates. Given the same descriptor sequence
/// [`DictWriter`] would intern (in first-intern order), `finish` produces byte-identical output:
/// a single `terms-0.dict` extent of `u32 LE length ‖ descriptor` records. The format carries
/// no record count or total size, and [`DictWriter`] never splits extents, so nothing needs
/// buffering beyond the `BufWriter`.
///
/// `append` is infallible by signature (mirroring [`DictWriter::intern`]); an IO error it hits
/// is held and returned by [`DictStreamWriter::finish`], which must be called for errors to be
/// observed. After an error, later `append`s still assign sequential ids but write nothing.
pub struct DictStreamWriter {
    dict_path: PathBuf,
    writer: Option<BufWriter<File>>,
    pending_err: Option<io::Error>,
    next_term_id: u32,
}

impl DictStreamWriter {
    /// Create a streaming dictionary writer for the given directory. The extent file is created
    /// on first `append` (or at `finish`, so an empty dictionary still writes an empty extent,
    /// as [`DictWriter`] does).
    pub fn new(dir: &Path) -> Self {
        DictStreamWriter {
            dict_path: dir.join("terms-0.dict"),
            writer: None,
            pending_err: None,
            next_term_id: 0,
        }
    }

    /// Append the next descriptor and return its term ID (sequential from 0). The caller
    /// guarantees distinctness and term-id order; violations are not detected here — a duplicate
    /// would silently get a fresh id, which [`DictWriter`] would not have assigned.
    pub fn append(&mut self, descriptor: &[u8]) -> TermId {
        let term_id = TermId::new(self.next_term_id);
        self.next_term_id += 1;
        if self.pending_err.is_none() {
            if let Err(e) = self.write_record(descriptor) {
                self.pending_err = Some(e);
            }
        }
        term_id
    }

    fn write_record(&mut self, descriptor: &[u8]) -> io::Result<()> {
        let writer = match self.writer {
            Some(ref mut w) => w,
            None => self
                .writer
                .insert(BufWriter::new(File::create(&self.dict_path)?)),
        };
        // The extent record length field is u32; a descriptor over u32::MAX bytes cannot be
        // represented and must fail closed, not truncate.
        let len = u32::try_from(descriptor.len()).map_err(|_| {
            io::Error::new(
                io::ErrorKind::InvalidInput,
                format!(
                    "dictionary descriptor of {} bytes exceeds the u32 record length field",
                    descriptor.len()
                ),
            )
        })?;
        writer.write_all(&len.to_le_bytes())?;
        writer.write_all(descriptor)?;
        Ok(())
    }

    /// The number of descriptors appended so far — equivalently, the next term ID that would be
    /// assigned. Same sizing contract as [`DictWriter::len`].
    pub fn len(&self) -> u32 {
        self.next_term_id
    }

    /// Whether nothing has been appended yet.
    pub fn is_empty(&self) -> bool {
        self.next_term_id == 0
    }

    /// Finish writing: flush the extent and return the paths written. Surfaces any IO error
    /// deferred from `append`.
    pub fn finish(mut self) -> io::Result<Vec<PathBuf>> {
        if let Some(e) = self.pending_err.take() {
            return Err(e);
        }
        match self.writer.take() {
            Some(mut writer) => writer.flush()?,
            // No append happened; DictWriter still writes an (empty) extent file.
            None => {
                File::create(&self.dict_path)?;
            }
        }
        Ok(vec![self.dict_path])
    }
}

/// Loads and queries a dictionary.
pub struct Dict {
    lookup_map: FxHashMap<Box<[u8]>, TermId>,
    len: u32,
}

impl Dict {
    /// Load a dictionary from the given paths (ordinal across extents in order).
    pub fn load(paths: &[PathBuf]) -> io::Result<Dict> {
        let mut lookup_map = FxHashMap::default();
        let mut term_id: u32 = 0;

        for path in paths {
            let data = std::fs::read(path)?;
            let mut offset = 0;

            while offset < data.len() {
                if offset + 4 > data.len() {
                    return Err(io::Error::new(
                        io::ErrorKind::InvalidData,
                        "incomplete length field",
                    ));
                }

                let len_bytes = [
                    data[offset],
                    data[offset + 1],
                    data[offset + 2],
                    data[offset + 3],
                ];
                let len = u32::from_le_bytes(len_bytes) as usize;
                offset += 4;

                if offset + len > data.len() {
                    return Err(io::Error::new(
                        io::ErrorKind::InvalidData,
                        "incomplete descriptor",
                    ));
                }

                let descriptor = &data[offset..offset + len];
                lookup_map.insert(descriptor.to_vec().into_boxed_slice(), TermId::new(term_id));
                term_id += 1;
                offset += len;
            }
        }

        Ok(Dict {
            len: term_id,
            lookup_map,
        })
    }

    /// A dictionary covering this one's descriptors plus the extents at `paths`, whose ordinals
    /// continue from `self.len()`.
    ///
    /// **Existing ordinals are preserved, and that is a correctness property rather than an
    /// efficiency one.** A session's granted terms are resolved to ordinals once, at authorise,
    /// and never re-resolved; renumbering an existing term would leave every session authorised
    /// before the extension evaluating against a *different* term than the one it was granted.
    /// Preservation is structural — the extents append and ordinals are assignment order — and
    /// the caller-visible test asserts it anyway, because it is what a flush's patch-equals-a-
    /// rebuild argument rests on.
    ///
    /// A descriptor already present is *not* re-interned: the extent's copy is skipped and the
    /// original ordinal stands, so a promotion that races another promotion of the same
    /// descriptor cannot produce two ids for one term.
    pub fn load_extending(&self, paths: &[PathBuf]) -> io::Result<Dict> {
        let extension = Dict::load(paths)?;
        let mut lookup_map = self.lookup_map.clone();
        let mut len = self.len;
        // Extent order is ordinal order, so walk it in that order rather than iterating the
        // extension's own (unordered) map, or the ids assigned here would depend on hash order.
        let mut by_ordinal: Vec<(&Box<[u8]>, TermId)> = extension
            .lookup_map
            .iter()
            .map(|(d, &id)| (d, id))
            .collect();
        by_ordinal.sort_unstable_by_key(|(_, id)| id.raw());
        for (descriptor, _) in by_ordinal {
            if lookup_map.contains_key(descriptor) {
                continue;
            }
            lookup_map.insert(descriptor.clone(), TermId::new(len));
            len += 1;
        }
        Ok(Dict { lookup_map, len })
    }

    /// Look up a descriptor and return its term ID if present.
    pub fn lookup(&self, descriptor: &[u8]) -> Option<TermId> {
        self.lookup_map.get(descriptor).copied()
    }

    /// Return the number of distinct terms.
    pub fn len(&self) -> u32 {
        self.len
    }

    /// Check if the dictionary is empty.
    pub fn is_empty(&self) -> bool {
        self.len == 0
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;
    use tempfile::TempDir;

    #[test]
    fn test_dict_writer_and_loader() {
        // Step 1: Failing test for intern, finish, and load
        let temp = TempDir::new().unwrap();
        let temp_path = temp.path();

        // Intern descriptors: [b"1207", b"9", b"1207"] should give ids [0, 1, 0]
        let mut writer = DictWriter::new(temp_path);
        let id1 = writer.intern(b"1207");
        let id2 = writer.intern(b"9");
        let id3 = writer.intern(b"1207");

        assert_eq!(id1.raw(), 0, "First intern of b'1207' should be 0");
        assert_eq!(id2.raw(), 1, "First intern of b'9' should be 1");
        assert_eq!(
            id3.raw(),
            0,
            "Second intern of b'1207' should deduplicate to 0"
        );

        // Finish and get the paths
        let paths = writer.finish().unwrap();
        assert_eq!(paths.len(), 1, "Should write one extent");
        assert_eq!(
            paths[0].file_name().unwrap(),
            "terms-0.dict",
            "File should be named terms-0.dict"
        );

        // Check raw bytes match the test vector exactly
        let raw_bytes = fs::read(&paths[0]).unwrap();
        let expected = vec![
            0x04, 0x00, 0x00, 0x00, // u32 LE 4
            0x31, 0x32, 0x30, 0x37, // "1207"
            0x01, 0x00, 0x00, 0x00, // u32 LE 1
            0x39, // "9"
        ];
        assert_eq!(raw_bytes, expected, "Raw bytes must match test vector");

        // Load the dictionary
        let dict = Dict::load(&paths).unwrap();

        // Verify the loaded dictionary
        assert_eq!(
            dict.lookup(b"9"),
            Some(TermId::new(1)),
            "lookup(b'9') should return TermId(1)"
        );
        assert_eq!(
            dict.lookup(b"nope"),
            None,
            "lookup(b'nope') should return None"
        );
        assert_eq!(dict.len(), 2, "Dictionary should have 2 distinct terms");
        assert_eq!(
            dict.lookup(b"1207"),
            Some(TermId::new(0)),
            "lookup(b'1207') should return TermId(0)"
        );
    }
}
