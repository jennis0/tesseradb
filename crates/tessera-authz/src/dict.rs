use rustc_hash::FxHashMap;
use std::fs::File;
use std::io::{self, BufWriter, Write};
use std::path::{Path, PathBuf};
use tessera_types::TermId;

/// The reserved access label every principal holds, and the term id it is interned at
/// (`per-point-attributes.md` §3.8, `configuration.md` §1).
///
/// **Reserved at build and satisfied at authorise, and the two halves must be read together.**
/// Every build interns this descriptor first, so term `0` is `public` in every bundle and is minted
/// for no other descriptor; every session resolved by the engine gains it *by construction*, inside
/// the trust boundary. It is deliberately neither a grant — which would make the corpus's one
/// universal label depend on grant hygiene — nor a plugin behaviour, the plugin being
/// caller-supplied code that decides what a credential's bytes mean.
pub const PUBLIC_LABEL: &[u8] = b"public";

/// [`PUBLIC_LABEL`]'s term id. `0` is not an *absent* sentinel in term space (that convention is
/// the category code space's); it is the first ordinal a dictionary assigns, and reserving it is
/// what makes the label's identity a property of the format rather than of the input.
pub const PUBLIC_TERM: TermId = TermId::new(0);

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

/// Concatenate `inputs` into one extent at `out`, and return how many records it carries.
///
/// **Ordinal-preserving by construction, which is the whole of its correctness argument.** A
/// dictionary ordinal is a position in the concatenation of `dict_extents` in listed order
/// ([`Dict::load`]), so writing the same records in the same order under one name changes no
/// ordinal — provided the caller replaces a *contiguous* range of the list, in place. A caller
/// that reordered the list, or coalesced a non-contiguous selection, would renumber every ordinal
/// after the gap, and a session's granted terms are resolved once at authorise and never
/// re-resolved: it would evaluate against a different term than the one it was granted.
///
/// **The records are walked, not the bytes copied**, so a truncated or corrupt extent fails here
/// rather than at the next `Engine::open` — the same walk `Dict::load` performs, at the one point
/// where a bad extent can still be left unpublished.
///
/// Duplicates are *not* removed. [`Dict::load`] skips a repeat without advancing the ordinal
/// counter, so a repeat that somehow existed already costs an ordinal in neither form; removing it
/// here would make this function's output disagree with its input for a reader that predates that
/// rule. Decision 0042 keeps the writer from producing one at all.
pub fn coalesce_dict_extents(inputs: &[PathBuf], out: &Path) -> io::Result<u64> {
    let mut writer = BufWriter::new(File::create(out)?);
    let mut records: u64 = 0;
    for path in inputs {
        let data = std::fs::read(path)?;
        let mut offset = 0;
        while offset < data.len() {
            if offset + 4 > data.len() {
                return Err(io::Error::new(
                    io::ErrorKind::InvalidData,
                    format!(
                        "dictionary extent {}: incomplete length field",
                        path.display()
                    ),
                ));
            }
            let len = u32::from_le_bytes([
                data[offset],
                data[offset + 1],
                data[offset + 2],
                data[offset + 3],
            ]) as usize;
            offset += 4;
            if offset + len > data.len() {
                return Err(io::Error::new(
                    io::ErrorKind::InvalidData,
                    format!(
                        "dictionary extent {}: incomplete descriptor",
                        path.display()
                    ),
                ));
            }
            writer.write_all(&data[offset - 4..offset + len])?;
            offset += len;
            records += 1;
        }
    }
    writer.flush()?;
    Ok(records)
}

/// Loads and queries a dictionary.
pub struct Dict {
    lookup_map: FxHashMap<Box<[u8]>, TermId>,
    len: u32,
}

impl Dict {
    /// Load a dictionary from the given paths (ordinal across extents in order).
    ///
    /// **A descriptor already seen is skipped, and the ordinal counter does not advance for it.**
    /// This is the reader half of the no-duplicate rule, and it exists because the alternative is
    /// a silent cross-compartment disclosure rather than untidiness. Without it this function and
    /// [`Dict::load_extending`] disagree about a repeated descriptor — `load_extending` skips it,
    /// this counted it — and *every* ordinal after the repeat shifts by one. The disagreement is
    /// between a running process and the same bundle after a restart: a tier's postings written
    /// under the in-memory ordinal `k` are read back as some other descriptor's, so a session
    /// granted one term is served another's items, with no error anywhere.
    ///
    /// The law this restores, and which the test alongside asserts:
    ///
    /// ```text
    /// load(a ++ b)  ≡  load(a).load_extending(b)
    /// ```
    ///
    /// `len` is therefore the count of **distinct** descriptors rather than of records, which
    /// differs only in the case a writer is forbidden to produce (`flush::promote` resolves
    /// against the live dictionary before interning anything).
    pub fn load(paths: &[PathBuf]) -> io::Result<Dict> {
        let mut lookup_map: FxHashMap<Box<[u8]>, TermId> = FxHashMap::default();
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
                offset += len;

                // Skip-if-present, not insert-and-count — see this function's doc.
                let key: Box<[u8]> = descriptor.to_vec().into_boxed_slice();
                if let std::collections::hash_map::Entry::Vacant(slot) = lookup_map.entry(key) {
                    slot.insert(TermId::new(term_id));
                    term_id += 1;
                }
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
        // Bounds order is ordinal order, so walk it in that order rather than iterating the
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

    /// The same extension as [`Dict::load_extending`], from descriptors already in memory — and
    /// the same skip-if-present rule, so the two agree on every input.
    ///
    /// **This exists so that a flush's live dictionary is not routed through a disk round-trip.**
    /// `flush::promote` assigns ordinals, writes the tier's postings under them, and writes the
    /// extent file; re-reading that file to build the dictionary it just described would add a
    /// failure mode rather than remove one. Nothing compares the two, so a bad read would silently
    /// *become* the live assignment while the tier holds the intended one. Restart equality is
    /// proved against the file by a test, which is where that obligation belongs.
    ///
    /// `descriptors` are appended in the order given, and that order is the caller's contract:
    /// they must be the same sequence, in the same order, that the extent file records.
    pub fn extended_with(&self, descriptors: &[Vec<u8>]) -> Dict {
        let mut lookup_map = self.lookup_map.clone();
        let mut len = self.len;
        for descriptor in descriptors {
            let key: Box<[u8]> = descriptor.clone().into_boxed_slice();
            if let std::collections::hash_map::Entry::Vacant(slot) = lookup_map.entry(key) {
                slot.insert(TermId::new(len));
                len += 1;
            }
        }
        Dict { lookup_map, len }
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

    fn extent_of(dir: &std::path::Path, descriptors: &[&[u8]]) -> Vec<PathBuf> {
        fs::create_dir_all(dir).unwrap();
        let mut writer = DictStreamWriter::new(dir);
        for d in descriptors {
            writer.append(d);
        }
        writer.finish().unwrap()
    }

    /// **`load(a ++ b) ≡ load(a).load_extending(b)`, including when `b` repeats a descriptor
    /// in `a`** — the promotion memo's obligation 3.
    ///
    /// This is the fail-open the reader rule closes, and it is worth stating what it was rather
    /// than only that it is fixed. `load` used to insert every record and count it, while
    /// `load_extending` skipped a descriptor the base already carried: with base `[a]` and extent
    /// `[a, b]` the running process had `a → 0, b → 1` and the same bundle reopened had
    /// `a → 1, b → 2`. Postings written under ordinal 1 are `b`'s, so after a restart a session
    /// granted `a` was served `b`'s items — silently, and across a compartment boundary.
    ///
    /// A duplicate is a writer bug (`flush::promote` resolves against the live dictionary before
    /// interning), so this asserts the *format* holds even when a writer does not.
    #[test]
    fn reload_equals_in_memory_extension_even_when_an_extent_repeats_a_descriptor() {
        let tmp = TempDir::new().unwrap();
        let base = extent_of(&tmp.path().join("base"), &[b"a"]);
        let ext = extent_of(&tmp.path().join("ext"), &[b"a", b"b"]);

        let in_memory = Dict::load(&base).unwrap().load_extending(&ext).unwrap();
        let concatenated: Vec<PathBuf> = base.iter().chain(ext.iter()).cloned().collect();
        let reloaded = Dict::load(&concatenated).unwrap();

        assert_eq!(in_memory.lookup(b"a"), reloaded.lookup(b"a"));
        assert_eq!(in_memory.lookup(b"b"), reloaded.lookup(b"b"));
        assert_eq!(in_memory.len(), reloaded.len());
        // Named exactly, so a future change cannot satisfy the equality by moving both.
        assert_eq!(reloaded.lookup(b"a"), Some(TermId::new(0)));
        assert_eq!(reloaded.lookup(b"b"), Some(TermId::new(1)));
        assert_eq!(reloaded.len(), 2);
    }

    /// [`Dict::extended_with`] is [`Dict::load_extending`] without the file — the equality
    /// `flush::promote` publishes on (memo §2 step 5).
    #[test]
    fn extending_from_memory_equals_extending_from_the_extent_it_wrote() {
        let tmp = TempDir::new().unwrap();
        let base = Dict::load(&extent_of(&tmp.path().join("base"), &[b"a", b"b"])).unwrap();
        let ext = extent_of(&tmp.path().join("ext"), &[b"c", b"d"]);

        let from_file = base.load_extending(&ext).unwrap();
        let from_memory = base.extended_with(&[b"c".to_vec(), b"d".to_vec()]);

        for d in [b"a".as_slice(), b"b", b"c", b"d"] {
            assert_eq!(from_memory.lookup(d), from_file.lookup(d), "{d:?}");
        }
        assert_eq!(from_memory.len(), from_file.len());
        assert_eq!(from_memory.lookup(b"c"), Some(TermId::new(2)));
    }

    /// Existing ordinals survive an extension — §3.4's premise 3, and the reason a session
    /// authorised before a promotion keeps evaluating the terms it was granted.
    #[test]
    fn extending_from_memory_preserves_every_existing_ordinal() {
        let tmp = TempDir::new().unwrap();
        let base = Dict::load(&extent_of(tmp.path(), &[b"a", b"b", b"c"])).unwrap();

        let extended = base.extended_with(&[b"c".to_vec(), b"d".to_vec()]);

        assert_eq!(extended.lookup(b"a"), Some(TermId::new(0)));
        assert_eq!(extended.lookup(b"b"), Some(TermId::new(1)));
        assert_eq!(
            extended.lookup(b"c"),
            Some(TermId::new(2)),
            "already present, so not re-interned"
        );
        assert_eq!(extended.lookup(b"d"), Some(TermId::new(3)));
        assert_eq!(extended.len(), 4);
    }
}
