use rustc_hash::FxHashMap;
use std::io;
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
