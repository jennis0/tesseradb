//! **The suggestion index** — a category vocabulary's values, arranged so that a typed prefix is a
//! contiguous range (`docs/design/value-suggestion.md` §6.1).
//!
//! # What it is, and what it deliberately is not
//!
//! One sorted index of §4's entries over **every** value of a vocabulary, visible to anybody or
//! not. It names no principal, holds no mask and is not a function of any session, so one copy
//! serves every session — which is decision 0093's cadence argument and is the whole reason the
//! per-request cost is a probe walk rather than a materialised set.
//!
//! **It is not a gate and cannot be one.** Everything about who may be told a value name is decided
//! by `Engine::suggest`'s predicate against the composed candidate, exactly as `Engine::categories`
//! decides it. A reader looking here for the disclosure control will not find it, and that is
//! correct: this file answers "which values could complete what was typed", and the answer includes
//! every value the asking principal must never hear of.
//!
//! # The six structures, and why not one
//!
//! [`tessera_filter::SortedDict`] carries no payload and refuses duplicate keys, and two values can
//! fold to one entry string — `cs.LG` and `CS.lg`, or two titles differing only in case — so the
//! index is six mapped files rather than one:
//!
//! | File | What it holds |
//! |---|---|
//! | `entries.dict` | the **distinct** folded entry strings, sorted; two binary searches turn a prefix into `[lo, hi)` |
//! | `runs.bin` | one `u32` per entry string plus a sentinel, indexing `payloads.bin`; entry `e`'s run is `runs[e]..runs[e + 1]` |
//! | `payloads.bin` | one 12-byte record per (entry string, value) pair, ordered within a run by (kind, key) |
//! | `codes.bin` | one `u32` per dense position: that value's vocabulary code |
//! | `by_code.bin` | the same pairs the other way round, sorted by code — the direction the per-session set's build reads (§6.3) |
//! | `strings.bin` + `offsets.bin` | the served keys and titles in position order |
//!
//! **The dense position, not the code, is the payload's identifier.** A code is drawn at random
//! over the declared width (per-point-attributes §3.4), so a code-indexed side array would need one
//! slot per code point — 4×10⁹ for a `u32` vocabulary. The dense position is the value's rank in
//! the vocabulary's key order, `0..V`, so every side array is exactly `V` long. It is also the unit
//! the per-session lever in §6.3 would want, should that ever be built.
//!
//! **The run structure makes the duplicate case ordinary rather than an error.** A prefix range is
//! a range of entry *strings*; the values under it are the concatenation of their runs.
//!
//! # Where the files live
//!
//! Under the engine's **local cache directory** — the one `Engine::open` already takes for the
//! fragment cache, which is engine-local and never inside the bundle (contracts §2.1 fixes a
//! bundle's contents). ⊘ The design says "the bundle's runtime directory"; no such directory
//! exists, and the fragment cache is the repository's one precedent for a derived, undigested,
//! rebuildable file the engine writes for itself. Nothing here is a build artefact: the manifest
//! does not name it, no digest covers it, and it is rebuilt from the vocabulary at every open —
//! which is what makes writing it outside the bundle correct rather than merely convenient.
//!
//! A build writes into a fresh numbered directory and the previous one is unlinked when the new
//! index is published. Unlinking a mapped file is safe on Linux: a live [`SuggestIndex`] keeps its
//! pages until it is dropped.
//!
//! # Cadence
//!
//! Built at `Engine::open`, held behind an `Arc` on the generation and **cloned across
//! publications**: a flush changes which entities carry a value and changes nothing this index
//! holds, so rebuilding per generation would pay the sort for no change. What the base index is
//! kept complete by between rebuilds is [`VocabularySuggest`]'s side map and retraction set.

use std::collections::BTreeMap;
use std::io;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use rayon::prelude::*;
use rustc_hash::{FxHashMap, FxHashSet};
use tessera_analyse::{EntryKind, SuggestionField, SuggestionFold};
use tessera_filter::{Access, SortedDict, SortedDictWriter};

/// The subdirectory of the engine's cache directory every vocabulary's index lives under.
///
/// **These files carry no format version, and deliberately.** Every other artefact here has one
/// because a second reader may hold an older copy; nothing ever reads one of these but the process
/// that wrote it. `Engine::open` deletes this whole directory before it builds, and a rebuild
/// writes a fresh numbered subdirectory, so a file from another version cannot be reached — the
/// checks in [`SuggestIndex::open`] are against a build this process interrupted, which is the only
/// wrong state expressible.
pub const SUGGEST_DIR: &str = "suggest";

/// One payload record: 12 bytes, `u32`-aligned so that the three fields are read without shifts.
const PAYLOAD_LEN: usize = 12;

/// One `by_code.bin` record: `(code, dense position)`, both `u32`, ascending by code.
///
/// **Interleaved rather than two parallel arrays**, because the search reads the code and then
/// wants the position beside it: two arrays would make every hit a second random touch.
const BY_CODE_LEN: usize = 8;

// ---------------------------------------------------------------------------------------------
// Mapped bytes
// ---------------------------------------------------------------------------------------------

/// A file's bytes, mapped where there are any.
///
/// `mmap` refuses a zero-length file, and three of the four side arrays are legitimately empty — a
/// vocabulary with no values, or one whose every value folds to nothing. An empty slice is the
/// honest answer for those, and making it a variant rather than a special-cased length keeps every
/// reader below written once.
#[derive(Debug)]
enum MappedBytes {
    Empty,
    Mapped(memmap2::Mmap),
}

impl MappedBytes {
    fn open(path: &Path) -> io::Result<Self> {
        let file = std::fs::File::open(path)?;
        if file.metadata()?.len() == 0 {
            return Ok(MappedBytes::Empty);
        }
        // SAFETY: the same argument every mapped artefact here makes — these files are written
        // once, by this process, before they are opened, and nothing writes them under a reader. A
        // rebuild writes a *different* directory and unlinks this one, which leaves a live mapping
        // intact rather than mutating it.
        let mapping = unsafe { memmap2::Mmap::map(&file) }?;
        Ok(MappedBytes::Mapped(mapping))
    }

    fn bytes(&self) -> &[u8] {
        match self {
            MappedBytes::Empty => &[],
            MappedBytes::Mapped(m) => m,
        }
    }
}

fn u32_at(bytes: &[u8], index: usize) -> u32 {
    let at = index * 4;
    u32::from_le_bytes(bytes[at..at + 4].try_into().expect("a 4-byte window"))
}

// ---------------------------------------------------------------------------------------------
// What a build is given
// ---------------------------------------------------------------------------------------------

/// One value of a vocabulary, as the index is built from it.
///
/// Taken in **key order** — the order [`tessera_store::vocabulary::VocabularyMinter::bindings`]
/// yields — because the position is that order's index and the payload sort's tie-break is by key.
#[derive(Debug, Clone)]
pub struct SuggestValue {
    pub key: String,
    pub title: Option<String>,
    pub code: u32,
}

/// One payload as the walk reads it: which value, where the match starts in the served string, and
/// which string that is.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Payload {
    /// The value's dense position.
    pub position: u32,
    /// Character offset into the served string this entry begins at.
    pub start: u32,
    pub kind: EntryKind,
    pub field: SuggestionField,
}

impl Payload {
    fn encode(&self) -> [u8; PAYLOAD_LEN] {
        let mut out = [0u8; PAYLOAD_LEN];
        out[0..4].copy_from_slice(&self.position.to_le_bytes());
        out[4..8].copy_from_slice(&self.start.to_le_bytes());
        let flags = (self.kind as u32) | ((self.field as u32) << 8);
        out[8..12].copy_from_slice(&flags.to_le_bytes());
        out
    }

    fn decode(bytes: &[u8]) -> io::Result<Payload> {
        let position = u32::from_le_bytes(bytes[0..4].try_into().expect("4 bytes"));
        let start = u32::from_le_bytes(bytes[4..8].try_into().expect("4 bytes"));
        let flags = u32::from_le_bytes(bytes[8..12].try_into().expect("4 bytes"));
        let kind = EntryKind::from_u8((flags & 0xff) as u8).ok_or_else(|| {
            malformed(format!("payload kind {} is not one of the three", flags & 0xff))
        })?;
        let field = match (flags >> 8) & 0xff {
            0 => SuggestionField::Key,
            1 => SuggestionField::Title,
            other => return Err(malformed(format!("payload field {other} is neither"))),
        };
        Ok(Payload {
            position,
            start,
            kind,
            field,
        })
    }
}

fn malformed(detail: impl Into<String>) -> io::Error {
    io::Error::new(io::ErrorKind::InvalidData, detail.into())
}

// ---------------------------------------------------------------------------------------------
// The index
// ---------------------------------------------------------------------------------------------

/// One vocabulary's suggestion index: the six mapped structures and the directory holding them.
#[derive(Debug)]
pub struct SuggestIndex {
    dir: PathBuf,
    entries: SortedDict,
    runs: MappedBytes,
    payloads: MappedBytes,
    codes: MappedBytes,
    by_code: MappedBytes,
    strings: MappedBytes,
    offsets: MappedBytes,
    /// The number of values — the length of `codes`, and half of `offsets` less its sentinel.
    values: u32,
}

impl SuggestIndex {
    /// Build the index for `values` into a fresh directory under `parent`.
    ///
    /// **The sort is the build cost**, measured at 34–38 s single-threaded for 22M key-plus-word-start
    /// entries at 10⁷ values against 2.4–4.2 s to write the dictionary (§6.1), so it runs on the
    /// caller's pool. The entries are sorted as `(offset, length)` references into one byte arena
    /// rather than as `String`s: the owned form measured a 2.46 GB peak at that scale.
    pub fn build(
        parent: &Path,
        generation: u64,
        values: &[SuggestValue],
        pool: &rayon::ThreadPool,
    ) -> io::Result<SuggestIndex> {
        let dir = parent.join(format!("{generation:06}"));
        std::fs::create_dir_all(&dir)?;

        let fold = SuggestionFold::new();
        // One arena of entry bytes plus one reference per entry. Derived per value in parallel and
        // concatenated, so the arena's order is the *value* order and not the sorted one — the
        // sort below reorders the references and never the bytes.
        let per_value: Vec<(Vec<u8>, Vec<EntryRef>)> = pool.install(|| {
            values
                .par_iter()
                .enumerate()
                .map(|(position, value)| {
                    let mut arena = Vec::new();
                    let mut refs = Vec::new();
                    for entry in fold.entries_of(&value.key, value.title.as_deref()) {
                        let at = arena.len() as u32;
                        arena.extend_from_slice(entry.entry.as_bytes());
                        refs.push(EntryRef {
                            at,
                            len: entry.entry.len() as u32,
                            position: position as u32,
                            start: entry.start,
                            kind: entry.kind,
                            field: entry.field,
                        });
                    }
                    (arena, refs)
                })
                .collect()
        });

        let mut arena: Vec<u8> = Vec::with_capacity(per_value.iter().map(|(a, _)| a.len()).sum());
        let mut refs: Vec<EntryRef> = Vec::with_capacity(per_value.iter().map(|(_, r)| r.len()).sum());
        for (chunk, mut chunk_refs) in per_value {
            let base = arena.len() as u32;
            arena.extend_from_slice(&chunk);
            for entry in &mut chunk_refs {
                entry.at += base;
            }
            refs.append(&mut chunk_refs);
        }

        // **§7's total order, fixed at build**: ascending by folded entry string, ties by entry kind
        // (key, then title, then word start), then by key — which the dense position *is*, the
        // values having arrived in key order.
        pool.install(|| {
            refs.par_sort_unstable_by(|a, b| {
                arena[a.range()]
                    .cmp(&arena[b.range()])
                    .then(a.kind.cmp(&b.kind))
                    .then(a.position.cmp(&b.position))
            })
        });

        // Write the three entry-space files in one pass over the sorted references.
        let mut dict = SortedDictWriter::new(std::io::BufWriter::new(std::fs::File::create(
            dir.join("entries.dict"),
        )?))?;
        let mut runs: Vec<u8> = Vec::new();
        let mut payloads: Vec<u8> = Vec::with_capacity(refs.len() * PAYLOAD_LEN);
        let mut previous: Option<&[u8]> = None;
        for entry in &refs {
            let bytes = &arena[entry.range()];
            if previous != Some(bytes) {
                let key = std::str::from_utf8(bytes).map_err(|e| {
                    malformed(format!("an entry string is not valid UTF-8: {e}"))
                })?;
                dict.push(key)?;
                runs.extend_from_slice(&((payloads.len() / PAYLOAD_LEN) as u32).to_le_bytes());
                previous = Some(bytes);
            }
            payloads.extend_from_slice(&entry.payload().encode());
        }
        // The sentinel, so the last run's extent is `runs[e]..runs[e + 1]` like every other's.
        runs.extend_from_slice(&((payloads.len() / PAYLOAD_LEN) as u32).to_le_bytes());
        dict.finish()?;

        // And the three value-space files.
        let mut codes: Vec<u8> = Vec::with_capacity(values.len() * 4);
        let mut strings: Vec<u8> = Vec::new();
        let mut offsets: Vec<u8> = Vec::with_capacity((values.len() * 2 + 1) * 4);
        for value in values {
            codes.extend_from_slice(&value.code.to_le_bytes());
            offsets.extend_from_slice(&(strings.len() as u32).to_le_bytes());
            strings.extend_from_slice(value.key.as_bytes());
            offsets.extend_from_slice(&(strings.len() as u32).to_le_bytes());
            if let Some(title) = &value.title {
                strings.extend_from_slice(title.as_bytes());
            }
        }
        offsets.extend_from_slice(&(strings.len() as u32).to_le_bytes());

        // **The reverse of `codes.bin`** — see [`Self::positions_of_ascending`] for what reads it
        // and why the direction is needed at all. Sorted by code, so the lookup is a binary search
        // and a run of ascending codes is a forward walk.
        let mut by_code: Vec<(u32, u32)> = values
            .iter()
            .enumerate()
            .map(|(position, value)| (value.code, position as u32))
            .collect();
        pool.install(|| by_code.par_sort_unstable());
        let mut by_code_bytes: Vec<u8> = Vec::with_capacity(by_code.len() * BY_CODE_LEN);
        for (code, position) in &by_code {
            by_code_bytes.extend_from_slice(&code.to_le_bytes());
            by_code_bytes.extend_from_slice(&position.to_le_bytes());
        }

        std::fs::write(dir.join("by_code.bin"), &by_code_bytes)?;
        std::fs::write(dir.join("runs.bin"), &runs)?;
        std::fs::write(dir.join("payloads.bin"), &payloads)?;
        std::fs::write(dir.join("codes.bin"), &codes)?;
        std::fs::write(dir.join("strings.bin"), &strings)?;
        std::fs::write(dir.join("offsets.bin"), &offsets)?;

        Self::open(dir, values.len() as u32)
    }

    /// Map a directory a [`Self::build`] just wrote, checking the four arrays against each other.
    ///
    /// The checks are the format's own — no digest covers these files, and none should: they are
    /// derived from the vocabulary in this process and rebuilt at every open. What they catch is a
    /// build interrupted part-way and a directory left by an earlier format version, both of which
    /// would otherwise be read as data.
    fn open(dir: PathBuf, values: u32) -> io::Result<SuggestIndex> {
        let entries = SortedDict::open(&dir.join("entries.dict"), Access::Mapped)
            .map_err(io::Error::from)?;
        let index = SuggestIndex {
            runs: MappedBytes::open(&dir.join("runs.bin"))?,
            payloads: MappedBytes::open(&dir.join("payloads.bin"))?,
            codes: MappedBytes::open(&dir.join("codes.bin"))?,
            by_code: MappedBytes::open(&dir.join("by_code.bin"))?,
            strings: MappedBytes::open(&dir.join("strings.bin"))?,
            offsets: MappedBytes::open(&dir.join("offsets.bin"))?,
            entries,
            values,
            dir,
        };
        let runs = index.runs.bytes();
        if runs.len() != (index.entries.len() as usize + 1) * 4 {
            return Err(malformed(format!(
                "the run-start array is {} bytes for {} entry strings",
                runs.len(),
                index.entries.len()
            )));
        }
        if !index.payloads.bytes().len().is_multiple_of(PAYLOAD_LEN) {
            return Err(malformed("the payload array is not a whole number of records"));
        }
        if !index.entries.is_empty()
            && u32_at(runs, index.entries.len() as usize) as usize * PAYLOAD_LEN
                != index.payloads.bytes().len()
        {
            return Err(malformed("the run sentinel does not name the payload array's end"));
        }
        if index.codes.bytes().len() != values as usize * 4 {
            return Err(malformed("the code array does not have one entry per value"));
        }
        if index.by_code.bytes().len() != values as usize * BY_CODE_LEN {
            return Err(malformed("the code→position array does not have one record per value"));
        }
        if index.offsets.bytes().len() != (values as usize * 2 + 1) * 4 {
            return Err(malformed("the string offsets do not have two entries per value"));
        }
        Ok(index)
    }

    /// The directory this index's files live in, so a superseding build can unlink it.
    pub fn dir(&self) -> &Path {
        &self.dir
    }

    /// How many values the vocabulary held when this index was built.
    pub fn values(&self) -> u32 {
        self.values
    }

    /// The number of distinct folded entry strings.
    pub fn entry_count(&self) -> u32 {
        self.entries.len()
    }

    /// The number of (entry string, value) pairs.
    pub fn payload_count(&self) -> u32 {
        (self.payloads.bytes().len() / PAYLOAD_LEN) as u32
    }

    /// The entry-string range a folded prefix names. An empty prefix is the whole index, which is
    /// the honest reading and is the picker's initial list (§4).
    pub fn prefix_range(&self, folded: &str) -> io::Result<std::ops::Range<u32>> {
        self.entries.prefix_range(folded).map_err(io::Error::from)
    }

    /// The payload range an entry-string range names.
    pub fn payload_range(&self, entries: std::ops::Range<u32>) -> std::ops::Range<u32> {
        let runs = self.runs.bytes();
        if runs.is_empty() || entries.start >= entries.end {
            return 0..0;
        }
        u32_at(runs, entries.start as usize)..u32_at(runs, entries.end as usize)
    }

    /// The entry string at an ordinal, for the merge with the side map.
    pub fn entry_of<'s>(&self, ordinal: u32, scratch: &'s mut Vec<u8>) -> io::Result<&'s str> {
        self.entries.key_of(ordinal, scratch).map_err(io::Error::from)
    }

    pub fn payload_at(&self, index: u32) -> io::Result<Payload> {
        let at = index as usize * PAYLOAD_LEN;
        let bytes = self.payloads.bytes();
        if at + PAYLOAD_LEN > bytes.len() {
            return Err(malformed(format!("payload {index} is past the array")));
        }
        Payload::decode(&bytes[at..at + PAYLOAD_LEN])
    }

    /// The vocabulary code at a dense position.
    pub fn code_at(&self, position: u32) -> io::Result<u32> {
        if position >= self.values {
            return Err(malformed(format!("position {position} is past the value set")));
        }
        Ok(u32_at(self.codes.bytes(), position as usize))
    }

    /// **The dense positions of an ascending, distinct run of codes**, in the order given —
    /// `codes.bin` read backwards, and the one thing §6.3's set build needs that the walk does not.
    ///
    /// A code the vocabulary does not hold is **skipped, not refused**: a value minted since this
    /// index was built lives in the side map and has no position here, and its entities are in the
    /// same extents the sweep walks. Such a value is simply not in the set, and the walk takes the
    /// probe route for it (§6.3) — the fail-closed direction, since a value missing from the set is
    /// withheld rather than offered.
    ///
    /// **Ascending input, exponential search from where the last one landed.** The build hands over
    /// millions of codes in one call, so a binary search each would pay `log V` cache misses per
    /// code where a warm-started gallop pays about one; for a dense run it degenerates to the
    /// linear merge that shape wants anyway. `codes` must ascend — an unordered call silently
    /// finds fewer positions than it should, which withholds values rather than offering them, and
    /// [`Self::positions_of_ascending`]'s one caller sorts.
    pub fn positions_of_ascending(&self, codes: &[u32], mut found: impl FnMut(u32)) {
        let bytes = self.by_code.bytes();
        let n = self.values as usize;
        let code_at = |at: usize| u32_at(bytes, at * 2);
        let position_at = |at: usize| u32_at(bytes, at * 2 + 1);

        let mut at = 0usize;
        for &code in codes {
            // The first index at or after `at` whose code is not below `code`.
            let mut lo = at;
            let mut step = 1usize;
            while lo < n && code_at(lo) < code {
                let hi = (lo + step).min(n);
                if hi < n && code_at(hi) < code {
                    lo = hi;
                    step *= 2;
                    continue;
                }
                let (mut a, mut b) = (lo + 1, hi);
                while a < b {
                    let mid = a + (b - a) / 2;
                    if code_at(mid) < code {
                        a = mid + 1;
                    } else {
                        b = mid;
                    }
                }
                lo = a;
                break;
            }
            at = lo;
            if at >= n {
                break;
            }
            if code_at(at) == code {
                found(position_at(at));
            }
        }
    }

    /// The served key and title at a dense position — the strings a response carries, out of the
    /// index rather than out of a walk of the minter's map.
    pub fn served(&self, position: u32) -> io::Result<(&str, Option<&str>)> {
        if position >= self.values {
            return Err(malformed(format!("position {position} is past the value set")));
        }
        let offsets = self.offsets.bytes();
        let blob = self.strings.bytes();
        let key_at = u32_at(offsets, position as usize * 2) as usize;
        let title_at = u32_at(offsets, position as usize * 2 + 1) as usize;
        let end = u32_at(offsets, position as usize * 2 + 2) as usize;
        if !(key_at <= title_at && title_at <= end && end <= blob.len()) {
            return Err(malformed(format!(
                "the string offsets at position {position} do not ascend inside the blob"
            )));
        }
        let key = std::str::from_utf8(&blob[key_at..title_at])
            .map_err(|e| malformed(format!("key at position {position} is not UTF-8: {e}")))?;
        let title = if title_at == end {
            None
        } else {
            Some(
                std::str::from_utf8(&blob[title_at..end]).map_err(|e| {
                    malformed(format!("title at position {position} is not UTF-8: {e}"))
                })?,
            )
        };
        Ok((key, title))
    }
}

/// One entry, as the sort sees it: a reference into the arena plus what the payload will carry.
#[derive(Debug, Clone, Copy)]
struct EntryRef {
    at: u32,
    len: u32,
    position: u32,
    start: u32,
    kind: EntryKind,
    field: SuggestionField,
}

impl EntryRef {
    fn range(&self) -> std::ops::Range<usize> {
        self.at as usize..(self.at + self.len) as usize
    }

    fn payload(&self) -> Payload {
        Payload {
            position: self.position,
            start: self.start,
            kind: self.kind,
            field: self.field,
        }
    }
}

// ---------------------------------------------------------------------------------------------
// The live index: a base, the mints since it was built, and what an amendment withdrew
// ---------------------------------------------------------------------------------------------

/// One value in the side map — a mint, or an amendment's replacement entries.
///
/// It carries its served strings because it has no dense position: the base index's blob was
/// written before this value existed.
#[derive(Debug, Clone)]
pub struct SideValue {
    pub code: u32,
    pub key: Arc<str>,
    pub title: Option<Arc<str>>,
    pub start: u32,
    pub kind: EntryKind,
    pub field: SuggestionField,
    /// When this value arrived, counted per vocabulary.
    ///
    /// **What a rebuild keeps is decided by this and by nothing else.** A rebuild is dispatched
    /// against a snapshot of the vocabulary and lands some time later, by which point the side map
    /// may hold values the snapshot did not — so publication keeps exactly the values that arrived
    /// at or after the sequence the dispatch recorded. Deciding it by "is this code in the new
    /// base" instead would need the base's whole code set in memory to answer, for a question one
    /// counter answers exactly.
    pub seq: u64,
}

/// One vocabulary's live suggestion index: the mapped base, the mints since it was built, and the
/// values whose base entries an amendment has withdrawn.
///
/// **Cloned per publication, rebuilt only when values or titles change.** A flush changes which
/// entities carry a value and changes nothing here, so a per-generation rebuild would pay §6.1's
/// sort for a change it cannot see. Cloning is cheap: the base is an `Arc` and the two live
/// structures hold only what has arrived since it was built.
#[derive(Debug, Clone)]
pub struct VocabularySuggest {
    base: Arc<SuggestIndex>,
    /// Folded entry string → the values it names, ordered as a run is: by (kind, key).
    ///
    /// **Fed at the mint**, so a value minted by an ingest is suggestible on the next keystroke
    /// rather than at the next rebuild. A `BTreeMap` because the walk ranges it by exactly the
    /// prefix that ranges the base, and merges the two in folded order.
    side: BTreeMap<String, Vec<SideValue>>,
    /// Codes whose entries in the **base** the walk must skip.
    ///
    /// The only producer is a title amendment: the value's replacement entries go in `side` and its
    /// stale ones — the title entry, and the word starts derived from the title or from the key
    /// standing in for one — are withdrawn here until the rebuild lands. Its key entry is withdrawn
    /// with them and restated in `side`, because retracting the *whole* value is one rule where
    /// retracting three of its four entry kinds is three.
    ///
    /// ⊘ **Nothing amends a title at runtime today**: `titles` reaches a `VocabularyMinter` only
    /// through `seed_manifest`, so a changed title arrives with a new bundle and therefore with a
    /// rebuild. The machinery is here because the walk must consult *something* — a walk written
    /// without it would have to be rewritten when the verb arrives, and this is the half that is
    /// invariant-shaped.
    retracted: FxHashMap<u32, u64>,
    /// The sequence the next arrival takes. See [`SideValue::seq`].
    next_seq: u64,
    /// Set when a value or title change has landed in `side`/`retracted` that a rebuild should
    /// fold into the base.
    rebuild_wanted: bool,
}

impl VocabularySuggest {
    pub fn new(base: Arc<SuggestIndex>) -> Self {
        VocabularySuggest {
            base,
            side: BTreeMap::new(),
            retracted: FxHashMap::default(),
            next_seq: 0,
            rebuild_wanted: false,
        }
    }

    /// **The successor after a rebuild landed**: the new base, plus every arrival the rebuild's
    /// snapshot did not see.
    ///
    /// `covered_through` is the sequence [`Self::next_seq`] stood at when the rebuild was
    /// dispatched. Everything below it is in the new base and is dropped from the side map;
    /// everything at or above it arrived while the sort ran and is kept, which is what makes the
    /// rebuild's latency invisible rather than a window in which a minted value is not suggested.
    pub fn rebuilt(&self, base: Arc<SuggestIndex>, covered_through: u64) -> VocabularySuggest {
        let mut side: BTreeMap<String, Vec<SideValue>> = BTreeMap::new();
        for (entry, run) in &self.side {
            let kept: Vec<SideValue> = run
                .iter()
                .filter(|value| value.seq >= covered_through)
                .cloned()
                .collect();
            if !kept.is_empty() {
                side.insert(entry.clone(), kept);
            }
        }
        let retracted: FxHashMap<u32, u64> = self
            .retracted
            .iter()
            .filter(|(_, seq)| **seq >= covered_through)
            .map(|(code, seq)| (*code, *seq))
            .collect();
        let rebuild_wanted = !side.is_empty();
        VocabularySuggest {
            base,
            side,
            retracted,
            next_seq: self.next_seq,
            rebuild_wanted,
        }
    }

    /// The sequence a rebuild dispatched now would cover through.
    pub fn next_seq(&self) -> u64 {
        self.next_seq
    }

    pub fn base(&self) -> &Arc<SuggestIndex> {
        &self.base
    }

    /// Whether the base is behind the vocabulary — what a background rebuild watches.
    pub fn rebuild_wanted(&self) -> bool {
        self.rebuild_wanted
    }

    /// How many values the side map holds — the amount the base is behind by.
    pub fn side_len(&self) -> usize {
        self.side.values().map(Vec::len).sum()
    }

    /// **Record a value minted since the base was built.** Idempotent for a value already held.
    pub fn mint(&mut self, fold: &SuggestionFold, key: &str, title: Option<&str>, code: u32) {
        let seq = self.next_seq;
        self.next_seq += 1;
        let key: Arc<str> = Arc::from(key);
        let title: Option<Arc<str>> = title.map(Arc::from);
        for entry in fold.entries_of(&key, title.as_deref()) {
            let run = self.side.entry(entry.entry).or_default();
            let value = SideValue {
                code,
                key: Arc::clone(&key),
                title: title.clone(),
                start: entry.start,
                kind: entry.kind,
                field: entry.field,
                seq,
            };
            // Ordered as a base run is — by (kind, key) — so the merged walk is one order
            // throughout and §7 holds across the seam.
            match run.binary_search_by(|held| {
                held.kind.cmp(&value.kind).then(held.key.cmp(&value.key))
            }) {
                Ok(at) => run[at] = value,
                Err(at) => run.insert(at, value),
            }
        }
        self.rebuild_wanted = true;
    }

    /// **Amend a value's title**: withdraw its base entries and restate the whole value in the side
    /// map, then ask for a rebuild.
    ///
    /// ⊘ No runtime verb reaches this; see [`Self::retracted`]'s field doc.
    pub fn amend_title(
        &mut self,
        fold: &SuggestionFold,
        key: &str,
        title: Option<&str>,
        code: u32,
    ) {
        self.retracted.insert(code, self.next_seq);
        // The side map may already hold this value under its old entries; drop them before the new
        // ones go in, or a prefix matching only the old title would still find it.
        self.forget(code);
        self.mint(fold, key, title, code);
    }

    fn forget(&mut self, code: u32) {
        self.side.retain(|_, run| {
            run.retain(|value| value.code != code);
            !run.is_empty()
        });
    }

    /// The side map's entries under a folded prefix, in folded order.
    pub fn side_range<'a>(
        &'a self,
        folded: &str,
    ) -> impl Iterator<Item = (&'a String, &'a Vec<SideValue>)> {
        self.side
            .range(folded.to_string()..)
            .take_while({
                let folded = folded.to_string();
                move |(entry, _)| entry.starts_with(&folded)
            })
    }

    /// Whether the base's entries for `code` are withdrawn.
    pub fn is_retracted(&self, code: u32) -> bool {
        self.retracted.contains_key(&code)
    }
}

/// Every vocabulary's live index, by vocabulary name — what a [`crate::Generation`] carries.
///
/// **Per vocabulary, not per column**, because the index is over a value set and two columns may
/// share one (per-point-attributes §3.9). What is *not* shared is the gate: a principal who may see
/// `finance` under one column may see nothing under another, and that is decided per column at the
/// request, against member sets this structure knows nothing about.
#[derive(Debug, Default)]
pub struct SuggestIndexes {
    by_vocabulary: FxHashMap<String, VocabularySuggest>,
}

impl SuggestIndexes {
    pub fn get(&self, vocabulary: &str) -> Option<&VocabularySuggest> {
        self.by_vocabulary.get(vocabulary)
    }

    pub fn is_empty(&self) -> bool {
        self.by_vocabulary.is_empty()
    }

    pub fn names(&self) -> impl Iterator<Item = &str> {
        self.by_vocabulary.keys().map(String::as_str)
    }

    /// The same set without one vocabulary — the fault state
    /// [`crate::EngineError::SuggestionUnavailable`] exists for, which nothing request-shaped
    /// produces. Reached only from `Engine::forget_suggestion_index_for_test`.
    #[cfg(feature = "fault-injection")]
    pub fn without(&self, vocabulary: &str) -> SuggestIndexes {
        let mut by_vocabulary = self.by_vocabulary.clone();
        by_vocabulary.remove(vocabulary);
        SuggestIndexes { by_vocabulary }
    }

    /// **Build every vocabulary's index**, at `Engine::open`.
    ///
    /// A vocabulary whose index will not build is **omitted rather than fatal**, and the omission
    /// is loud. This is a derived structure and its absence costs a surface, not a disclosure: the
    /// suggest verb refuses a column whose vocabulary has no index, and every other surface is
    /// unaffected. Refusing to open the bundle for it would take a whole deployment down for a
    /// typeahead.
    pub fn build(
        parent: &Path,
        vocabularies: &tessera_store::vocabulary::Vocabularies,
        names: impl IntoIterator<Item = String>,
        pool: &rayon::ThreadPool,
    ) -> SuggestIndexes {
        let mut by_vocabulary = FxHashMap::default();
        for name in names {
            let Some(minter) = vocabularies.get(&name) else {
                continue;
            };
            let values = values_of(minter);
            match SuggestIndex::build(&parent.join(&name), 0, &values, pool) {
                Ok(index) => {
                    by_vocabulary.insert(name, VocabularySuggest::new(Arc::new(index)));
                }
                Err(source) => {
                    tracing::warn!(
                        vocabulary = %name,
                        %source,
                        "the suggestion index for this vocabulary would not build, so \
                         /v1/categories/{{column}}/suggest refuses every column drawing on it; \
                         every other surface over it is unaffected"
                    );
                }
            }
        }
        SuggestIndexes { by_vocabulary }
    }

    /// The successor after a window that minted, carrying every other vocabulary's index forward
    /// untouched.
    pub fn with_mints(
        self: &Arc<Self>,
        fold: &SuggestionFold,
        vocabularies: &tessera_store::vocabulary::Vocabularies,
        mints: &[(String, String, u32)],
    ) -> Arc<SuggestIndexes> {
        // **Every commit window publishes; almost none mints.** Taking the `Arc` rather than a
        // clone on that path means the steady state pays a pointer, where cloning the map would
        // pay a `BTreeMap` and a `HashMap` per vocabulary per window for a structure that did not
        // change.
        if mints.is_empty() {
            return Arc::clone(self);
        }
        let mut by_vocabulary = self.by_vocabulary.clone();
        for (vocabulary, key, code) in mints {
            let Some(live) = by_vocabulary.get_mut(vocabulary) else {
                continue;
            };
            let title = vocabularies
                .get(vocabulary)
                .and_then(|minter| minter.title_of(key))
                .map(str::to_string);
            live.mint(fold, key, title.as_deref(), *code);
        }
        Arc::new(SuggestIndexes { by_vocabulary })
    }
}

/// A vocabulary's values in key order — the order the dense position indexes.
pub fn values_of(minter: &tessera_store::vocabulary::VocabularyMinter) -> Vec<SuggestValue> {
    minter
        .bindings()
        .map(|(key, code)| SuggestValue {
            key: key.to_string(),
            title: minter.title_of(key).map(str::to_string),
            code,
        })
        .collect()
}

impl SuggestIndexes {
    /// The successor after one vocabulary's rebuild landed.
    pub fn with_rebuilt(
        &self,
        vocabulary: &str,
        base: Arc<SuggestIndex>,
        covered_through: u64,
    ) -> SuggestIndexes {
        let mut by_vocabulary = self.by_vocabulary.clone();
        if let Some(live) = by_vocabulary.get_mut(vocabulary) {
            *live = live.rebuilt(base, covered_through);
        }
        SuggestIndexes { by_vocabulary }
    }

    /// The vocabulary a rebuild is most owed to, or `None` where none is far enough behind.
    ///
    /// **A threshold, not "on every change", and the difference is the sort.** §6.1 asks for a
    /// rebuild "only when the vocabulary's values or titles change", which is the *necessary*
    /// condition rather than the whole rule: one mint per ingest batch against a 10⁷-value
    /// vocabulary would dispatch a 34–38-second sort per batch, and the result of every one but
    /// the last would be superseded before it landed. The side map is what makes waiting free —
    /// a value in it is suggested exactly as one in the base is — so the rebuild is owed to
    /// *residency*, and a threshold is the shape residency wants.
    pub fn most_owed_rebuild(&self) -> Option<(&str, u64)> {
        self.by_vocabulary
            .iter()
            .filter(|(_, live)| {
                live.rebuild_wanted() && live.side_len() >= SUGGEST_REBUILD_SIDE_VALUES
            })
            .max_by_key(|(_, live)| live.side_len())
            .map(|(name, live)| (name.as_str(), live.next_seq()))
    }
}

/// How far the side map is allowed to run ahead of the base before a rebuild is dispatched.
///
/// A side value is a `BTreeMap` entry per entry kind it contributes plus its two served strings, so
/// this is on the order of a megabyte of heap at the threshold — small beside the mapped base, and
/// large enough that a steadily minting ingest dispatches a rebuild in the hundreds of batches
/// rather than per batch.
pub const SUGGEST_REBUILD_SIDE_VALUES: usize = 4096;

/// A rebuild that finished, waiting for the executor to publish it.
pub struct CompletedSuggest {
    pub vocabulary: String,
    pub index: Arc<SuggestIndex>,
    /// The sequence the dispatch recorded — see [`SideValue::seq`].
    pub covered_through: u64,
}

// ---------------------------------------------------------------------------------------------
// The walk
// ---------------------------------------------------------------------------------------------

/// What the walk found for one value, before a response shape is put on it.
#[derive(Debug, Clone)]
pub struct Found {
    pub code: u32,
    pub key: String,
    pub title: Option<String>,
    pub field: SuggestionField,
    pub start: u32,
    pub len: u32,
    pub count: Option<u64>,
}

/// What bounds one walk.
#[derive(Debug, Clone, Copy)]
pub struct WalkBudget {
    /// The most values one page carries.
    pub limit: usize,
    /// The most values one request **examines** — a deployment constant, `max_suggestion_walk`.
    ///
    /// **It bounds latency, not disclosure.** The channel's bound is the vocabulary itself, which
    /// the enumeration walks whole and unbudgeted; a budget only ever narrows what one request
    /// examines (§6.2, §8).
    pub walk_budget: u64,
    pub counts: bool,
}

/// **One suggestion walk** (`docs/design/value-suggestion.md` §6.2), over the base index merged
/// with the vocabulary's side map.
///
/// Free of the engine so that the gate and the traversal are separable — `Engine::suggest` supplies
/// `visible` and `count`, and the bench supplies its own. Everything about *who may be told* is in
/// those two closures and in nothing here; this function would happily emit every value of the
/// vocabulary if handed a predicate that said yes.
///
/// `q` is the caller's text, folded here rather than by the caller, so the query and the index go
/// through one fold.
///
/// The error type is the caller's, so a refusal keeps the shape its surface owes; `unreadable` is
/// how an index read failure becomes one. **A read that fails part-way refuses the whole walk**
/// rather than serving what it found: refusing at the value the read failed at would make the
/// refusal a function of the prefix the caller typed (§3).
///
/// # The two routes
///
/// `set` is the session's own visible-value set where one has been built (§6.3, decision 0124), and
/// `None` where it has not — the first keystrokes on a session-column pair, a viewer wider than
/// `selection.max_suggest_set_entities`, a column with nothing to sweep, and every `public` column,
/// which has no predicate to answer.
///
/// **The two routes answer the same page**: the traversal, the order and the spans are identical,
/// and the only thing the set replaces is *how* a value's visibility is decided — a bit in the
/// caller's own set rather than a boolean probe against a memory-mapped posting. Where the set
/// answers, **no posting is read and no walk budget is spent**, so `more` means the page filled and
/// nothing else. A value the set cannot speak for — one minted since the sweep, which lives in the
/// side map and has no dense position — takes the probe route, which is why both are supplied.
#[allow(clippy::too_many_arguments)]
pub fn walk<E>(
    live: &VocabularySuggest,
    fold: &SuggestionFold,
    q: &str,
    budget: WalkBudget,
    visible: &dyn Fn(u32) -> Result<bool, E>,
    count: &dyn Fn(u32) -> Result<u64, E>,
    unreadable: &dyn Fn(io::Error) -> E,
    set: Option<&crate::suggest_set::SuggestSet>,
) -> Result<(Vec<Found>, bool), E> {
    let folded = fold.entry(q);
    let base = live.base();
    let entries = base.prefix_range(&folded).map_err(unreadable)?;

    let mut state = WalkState {
        found: Vec::new(),
        emitted: FxHashSet::default(),
        examined: 0,
        more: false,
        budget,
        set,
    };
    let side: Vec<(&str, &SideValue)> = live
        .side_range(&folded)
        .flat_map(|(entry, run)| run.iter().map(move |value| (entry.as_str(), value)))
        .collect();

    // Two shapes of one walk. **The side map is empty in the ordinary case** — a bundle nothing has
    // been ingested into since it opened — and then no entry string is decoded at all: the payload
    // array is walked directly, which is the whole reason the run starts are an array beside the
    // dictionary rather than inside it. Where the side map has entries the merge needs the base's
    // entry strings to order against, and pays a block decode per entry ordinal.
    if side.is_empty() {
        for index in base.payload_range(entries) {
            if state.stop() {
                break;
            }
            let payload = base.payload_at(index).map_err(unreadable)?;
            let code = base.code_at(payload.position).map_err(unreadable)?;
            if live.is_retracted(code) {
                continue;
            }
            // **The gate before the served strings**, which is where the ordering earns its keep: a
            // sparse viewer rejects better than 99.9% of what it examines, and the two mapped reads
            // and the UTF-8 validation below are paid only for what it keeps.
            if state.admits(code, Some(payload.position), visible)? {
                let (key, title) = base.served(payload.position).map_err(unreadable)?;
                state.emit(
                    fold, &folded, code, key, title, payload.field, payload.start, count,
                )?;
            }
        }
    } else {
        let mut side_at = 0usize;
        let mut scratch = Vec::new();
        'entries: for ordinal in entries {
            let entry = base.entry_of(ordinal, &mut scratch).map_err(unreadable)?;
            // Every side entry sorting before this one, first — the merge's whole content.
            while side_at < side.len() && side[side_at].0 < entry {
                if state.stop() {
                    break 'entries;
                }
                let value = side[side_at].1;
                side_at += 1;
                if state.admits(value.code, None, visible)? {
                    state.emit(
                        fold,
                        &folded,
                        value.code,
                        &value.key,
                        value.title.as_deref(),
                        value.field,
                        value.start,
                        count,
                    )?;
                }
            }
            for index in base.payload_range(ordinal..ordinal + 1) {
                if state.stop() {
                    break 'entries;
                }
                let payload = base.payload_at(index).map_err(unreadable)?;
                let code = base.code_at(payload.position).map_err(unreadable)?;
                let (key, title) = base.served(payload.position).map_err(unreadable)?;
                // A side value sharing this entry string and sorting ahead of this payload goes
                // first, so the two sources are one order rather than two — which is what makes
                // §7 hold across the seam.
                while side_at < side.len()
                    && side[side_at].0 == entry
                    && (side[side_at].1.kind, side[side_at].1.key.as_ref())
                        < (payload.kind, key)
                {
                    if state.stop() {
                        break 'entries;
                    }
                    let value = side[side_at].1;
                    side_at += 1;
                    if state.admits(value.code, None, visible)? {
                        state.emit(
                            fold,
                            &folded,
                            value.code,
                            &value.key,
                            value.title.as_deref(),
                            value.field,
                            value.start,
                            count,
                        )?;
                    }
                }
                if live.is_retracted(code) {
                    continue;
                }
                // The served key was already read here, the merge's ordering comparison needing
                // it — which is why this arm runs only where the side map has entries under the
                // prefix, and the arm above runs otherwise.
                if state.admits(code, Some(payload.position), visible)? {
                    state.emit(
                        fold, &folded, code, key, title, payload.field, payload.start, count,
                    )?;
                }
            }
        }
        // Whatever is left of the side map's range sorts after every base entry under it.
        while side_at < side.len() {
            if state.stop() {
                break;
            }
            let value = side[side_at].1;
            side_at += 1;
            if state.admits(value.code, None, visible)? {
                state.emit(
                    fold,
                    &folded,
                    value.code,
                    &value.key,
                    value.title.as_deref(),
                    value.field,
                    value.start,
                    count,
                )?;
            }
        }
    }
    Ok((state.found, state.more))
}

/// The walk's running state — the page, what it has already emitted, and what it has spent.
struct WalkState<'a> {
    found: Vec<Found>,
    /// Codes already on the page. **A value appears once**, at its first matching entry in §7's
    /// order, and its `match` reports that entry.
    emitted: FxHashSet<u32>,
    examined: u64,
    more: bool,
    budget: WalkBudget,
    /// The session's visible-value set where one has been built — see [`walk`]'s two routes.
    set: Option<&'a crate::suggest_set::SuggestSet>,
}

impl WalkState<'_> {
    /// Whether the walk is over, and — the same question — whether `more` is owed.
    ///
    /// Both stopping conditions set `more`, because both mean the range was not exhausted. Asked
    /// *before* each item rather than after, so a page that fills exactly as the range ends reports
    /// `more: false` rather than a truncation that did not happen.
    fn stop(&mut self) -> bool {
        if self.found.len() >= self.budget.limit || self.examined >= self.budget.walk_budget {
            self.more = true;
            return true;
        }
        false
    }

    /// **Does this value reach the page?** — the emitted check, the budget accounting, and the
    /// gate, in that order and before a single served byte is read.
    ///
    /// Separate from [`Self::emit`] because reading the served strings is a mapped read and a
    /// UTF-8 validation per value, and at the sparsest viewer measured the gate rejects better than
    /// 99.9% of what the walk examines. Fusing the two put that read on every rejection and cost a
    /// measured multiple of the walk at 10⁷ values.
    fn admits<E>(
        &mut self,
        code: u32,
        position: Option<u32>,
        visible: &dyn Fn(u32) -> Result<bool, E>,
    ) -> Result<bool, E> {
        if self.emitted.contains(&code) {
            return Ok(false);
        }
        // **The set route, where there is a set and the value has a dense position for it to be
        // over** (§6.3). It reads no posting, so it spends no budget: the budget bounds *probing*,
        // which is what §6.2 measures and §8 registers, and a bit test is neither the cost nor the
        // channel. That is what makes `more` exact here — the walk stops when the page fills and
        // for no other reason.
        //
        // A side-map value has no position and falls through to the probe: it was minted after the
        // sweep, so the set cannot speak for it either way, and asking the posting is the answer
        // that is right rather than merely available.
        if let (Some(set), Some(position)) = (self.set, position) {
            return Ok(set.contains(position));
        }
        // **One unit per gate test.** The budget bounds how many values one request *examines* —
        // the quantity §6.2 measures and §8 registers — so a value already on the page costs
        // nothing, and a value under the prefix by two entries and *not* on the page costs two:
        // it is probed twice, and what the budget bounds is the probing.
        self.examined += 1;
        visible(code)
    }

    #[allow(clippy::too_many_arguments)]
    fn emit<E>(
        &mut self,
        fold: &SuggestionFold,
        folded_q: &str,
        code: u32,
        key: &str,
        title: Option<&str>,
        field: SuggestionField,
        start: u32,
        count: &dyn Fn(u32) -> Result<u64, E>,
    ) -> Result<(), E> {
        let served = match field {
            SuggestionField::Key => key,
            SuggestionField::Title => title.unwrap_or(key),
        };
        let count = if self.budget.counts {
            Some(count(code)?)
        } else {
            None
        };
        self.emitted.insert(code);
        self.found.push(Found {
            code,
            key: key.to_string(),
            title: title.map(str::to_string),
            field,
            start,
            len: match_len(fold, served, start, folded_q),
            count,
        });
        Ok(())
    }
}

/// **`match.len`, derived at response time rather than stored** (§4).
///
/// The index records where an entry starts as a character offset into the *served* string; this
/// re-folds that string forward from there until `q`'s folded bytes are consumed, and the
/// characters consumed are the length. That is O(|q|) per emitted value — `q` is bounded at 256
/// bytes by the contract — and it needs no second copy of the folded text beside the entry, which
/// is what lets `match` be reported in characters of the string the client is about to draw rather
/// than of a folded form the client never sees.
///
/// An empty query consumes nothing and highlights nothing, which is the picker's initial list.
fn match_len(fold: &SuggestionFold, served: &str, start: u32, folded_q: &str) -> u32 {
    if folded_q.is_empty() {
        return 0;
    }
    let Some((at, _)) = served.char_indices().nth(start as usize) else {
        return 0;
    };
    let tail = &served[at..];
    let mut characters = 0u32;
    for (end, c) in tail.char_indices() {
        characters += 1;
        let folded = fold.entry(&tail[..end + c.len_utf8()]);
        if folded.len() >= folded_q.len() && folded.starts_with(folded_q) {
            return characters;
        }
    }
    // **The whole tail, where the loop consumed it without matching.** Reachable only from an
    // offset that does not name the word the entry came from — `SuggestionFold::served_word_starts`
    // pairs the folded and served boundary lists by position, and two cancelling boundary changes
    // can leave the counts equal over different boundaries (see that function). The entry string is
    // the fold's own output either way, so such a value still *matches* correctly and is still
    // gated correctly; what is wrong is only the span. Returning the tail's length rather than
    // panicking or refusing keeps it that way: an over-long highlight on a string the client was
    // going to draw anyway.
    characters
}

#[cfg(test)]
mod tests {
    use super::*;

    fn pool() -> rayon::ThreadPool {
        rayon::ThreadPoolBuilder::new()
            .num_threads(2)
            .build()
            .expect("a pool")
    }

    fn value(key: &str, title: Option<&str>, code: u32) -> SuggestValue {
        SuggestValue {
            key: key.to_string(),
            title: title.map(str::to_string),
            code,
        }
    }

    /// Values in key order, as `bindings()` yields them and as the dense position indexes.
    fn fixture() -> Vec<SuggestValue> {
        vec![
            value("CS.lg", None, 900),
            value("cs.AI", Some("Artificial Intelligence"), 41_000),
            value("cs.LG", Some("Machine Learning"), 41_207),
            value("machine_shop", None, 12),
            value("stat.ML", Some("Machine Learning (Statistics)"), 9),
        ]
    }

    /// Every (entry string, position) pair the index holds, in index order — the whole artefact,
    /// flattened, so a shape assertion is one comparison rather than a walk.
    fn flatten(index: &SuggestIndex) -> Vec<(String, u32, EntryKind)> {
        let mut out = Vec::new();
        let mut scratch = Vec::new();
        for ordinal in 0..index.entry_count() {
            let entry = index.entry_of(ordinal, &mut scratch).unwrap().to_string();
            for at in index.payload_range(ordinal..ordinal + 1) {
                let payload = index.payload_at(at).unwrap();
                out.push((entry.clone(), payload.position, payload.kind));
            }
        }
        out
    }

    /// **§7's total order, end to end**: ascending by folded entry string, ties by kind, then by
    /// key — which the dense position is.
    ///
    /// `CS.lg` and `cs.LG` fold to one entry string, which is the duplicate case the run structure
    /// exists for and which a plain `SortedDict` refuses outright.
    ///
    /// **Mutations this kills:** sorting by kind before the string; sorting positions before kinds;
    /// dropping the second value of a shared entry string; writing the run starts against the entry
    /// count instead of the payload count.
    #[test]
    fn the_index_is_one_sorted_order_over_every_entry() {
        let dir = tempfile::tempdir().unwrap();
        let index = SuggestIndex::build(dir.path(), 0, &fixture(), &pool()).unwrap();
        assert_eq!(
            flatten(&index),
            vec![
                // `CS.lg` and `cs.LG` share this entry string; the run holds both, in key order.
                ("cs.lg".to_string(), 0, EntryKind::Key),
                ("cs.lg".to_string(), 2, EntryKind::Key),
                // `.` is not a letter or a digit, so `lg` is a word start of the *key* — the
                // title-less value's word starts come from its key, "as the title would be".
                ("lg".to_string(), 0, EntryKind::WordStart),
                ("cs.ai".to_string(), 1, EntryKind::Key),
                ("artificial intelligence".to_string(), 1, EntryKind::Title),
                ("intelligence".to_string(), 1, EntryKind::WordStart),
                // A whole-title match precedes the word start derived from a longer title.
                ("machine learning".to_string(), 2, EntryKind::Title),
                ("machine learning (statistics)".to_string(), 4, EntryKind::Title),
                ("machine_shop".to_string(), 3, EntryKind::Key),
                ("shop".to_string(), 3, EntryKind::WordStart),
                ("statistics)".to_string(), 4, EntryKind::WordStart),
                ("stat.ml".to_string(), 4, EntryKind::Key),
                ("learning".to_string(), 2, EntryKind::WordStart),
                ("learning (statistics)".to_string(), 4, EntryKind::WordStart),
            ]
            .into_iter()
            .collect::<std::collections::BTreeSet<_>>()
            .into_iter()
            .collect::<Vec<_>>(),
            "the flattened index is not the sorted entry set"
        );
    }

    /// A prefix is a contiguous payload range, and the range is exactly the values under it.
    #[test]
    fn a_prefix_is_a_contiguous_payload_range() {
        let dir = tempfile::tempdir().unwrap();
        let index = SuggestIndex::build(dir.path(), 0, &fixture(), &pool()).unwrap();

        let positions = |q: &str| -> Vec<u32> {
            let entries = index.prefix_range(q).unwrap();
            index
                .payload_range(entries)
                .map(|at| index.payload_at(at).unwrap().position)
                .collect()
        };
        // Two values fold to `cs.lg`, and both are under `cs.`.
        assert_eq!(positions("cs."), vec![1, 0, 2]);
        // A word start, reached by a prefix of the word rather than of the title.
        assert_eq!(positions("lear"), vec![2, 4]);
        // A whole title and the word start of a longer one, in §7's order.
        assert_eq!(positions("machine"), vec![2, 4, 3]);
        assert!(positions("zzz").is_empty());
        // The empty query is the whole index, which is the picker's initial list.
        assert_eq!(positions("").len(), index.payload_count() as usize);
    }

    /// The served strings come out of the index rather than out of a walk of the minter's map, and
    /// a value with no title reads back as having none — never as an empty string.
    #[test]
    fn the_served_strings_round_trip_through_the_blob() {
        let dir = tempfile::tempdir().unwrap();
        let values = fixture();
        let index = SuggestIndex::build(dir.path(), 0, &values, &pool()).unwrap();
        for (position, value) in values.iter().enumerate() {
            let (key, title) = index.served(position as u32).unwrap();
            assert_eq!(key, value.key);
            assert_eq!(title, value.title.as_deref());
            assert_eq!(index.code_at(position as u32).unwrap(), value.code);
        }
        assert!(index.served(values.len() as u32).is_err());
        assert!(index.code_at(values.len() as u32).is_err());
    }

    /// A vocabulary with no values builds, opens and answers empty — the three side arrays are
    /// legitimately zero-length and `mmap` refuses a zero-length file.
    #[test]
    fn an_empty_vocabulary_builds_and_answers_empty() {
        let dir = tempfile::tempdir().unwrap();
        let index = SuggestIndex::build(dir.path(), 0, &[], &pool()).unwrap();
        assert_eq!(index.entry_count(), 0);
        assert_eq!(index.payload_count(), 0);
        assert!(index.payload_range(index.prefix_range("a").unwrap()).is_empty());
    }

    /// **A mint is suggestible before the rebuild lands**, which is what the side map is for.
    #[test]
    fn a_minted_value_is_in_the_side_range_at_once() {
        let dir = tempfile::tempdir().unwrap();
        let fold = SuggestionFold::new();
        let index = SuggestIndex::build(dir.path(), 0, &fixture(), &pool()).unwrap();
        let mut live = VocabularySuggest::new(Arc::new(index));
        assert_eq!(live.side_len(), 0);
        assert!(!live.rebuild_wanted());

        live.mint(&fold, "cs.CV", Some("Computer Vision"), 77);
        assert!(live.rebuild_wanted());
        // Three entries: the key, the title, and the title's one word start.
        assert_eq!(live.side_len(), 3);
        let under: Vec<&str> = live
            .side_range("vis")
            .flat_map(|(_, run)| run.iter().map(|v| &*v.key))
            .collect();
        assert_eq!(under, vec!["cs.CV"], "the word start is reachable");
        let under: Vec<&str> = live
            .side_range("cs.c")
            .flat_map(|(_, run)| run.iter().map(|v| &*v.key))
            .collect();
        assert_eq!(under, vec!["cs.CV"]);
        assert!(live.side_range("zz").next().is_none());
    }

    /// **A title amendment withdraws the value's base entries and restates it**, so the old title
    /// stops matching and the new one starts, with no rebuild in between.
    #[test]
    fn an_amendment_retracts_the_base_and_restates_the_value() {
        let dir = tempfile::tempdir().unwrap();
        let fold = SuggestionFold::new();
        let index = SuggestIndex::build(dir.path(), 0, &fixture(), &pool()).unwrap();
        let mut live = VocabularySuggest::new(Arc::new(index));

        live.amend_title(&fold, "cs.LG", Some("Statistical Learning"), 41_207);
        assert!(live.is_retracted(41_207));
        assert!(!live.is_retracted(41_000), "one value, not the vocabulary");
        let under: Vec<&str> = live
            .side_range("statistical")
            .flat_map(|(_, run)| run.iter().map(|v| &*v.key))
            .collect();
        assert_eq!(under, vec!["cs.LG"]);

        // Amending again drops the first amendment's entries rather than accumulating them.
        live.amend_title(&fold, "cs.LG", Some("Deep Learning"), 41_207);
        assert!(live.side_range("statistical").next().is_none());
        let under: Vec<&str> = live
            .side_range("deep")
            .flat_map(|(_, run)| run.iter().map(|v| &*v.key))
            .collect();
        assert_eq!(under, vec!["cs.LG"]);
    }

    /// **A rebuild keeps exactly what its snapshot did not see.** The sequence, not the new base's
    /// code set, is what decides — so a mint that lands while the sort runs is not lost, and one
    /// the sort covered is not held twice.
    #[test]
    fn a_rebuild_keeps_only_what_arrived_after_it_was_dispatched() {
        let dir = tempfile::tempdir().unwrap();
        let fold = SuggestionFold::new();
        let index = SuggestIndex::build(dir.path(), 0, &fixture(), &pool()).unwrap();
        let mut live = VocabularySuggest::new(Arc::new(index));

        live.mint(&fold, "cs.CV", Some("Computer Vision"), 77);
        // The dispatch's snapshot: everything minted so far is in the rebuild's input.
        let covered_through = live.next_seq();
        // …and this one arrives while the sort runs.
        live.mint(&fold, "cs.RO", Some("Robotics"), 78);

        let mut rebuilt_values = fixture();
        rebuilt_values.push(value("cs.CV", Some("Computer Vision"), 77));
        rebuilt_values.sort_by(|a, b| a.key.cmp(&b.key));
        let rebuilt = SuggestIndex::build(dir.path(), 1, &rebuilt_values, &pool()).unwrap();

        let live = live.rebuilt(Arc::new(rebuilt), covered_through);
        assert!(
            live.side_range("computer").next().is_none(),
            "the rebuild's own snapshot is folded in, not held twice"
        );
        let under: Vec<&str> = live
            .side_range("robot")
            .flat_map(|(_, run)| run.iter().map(|v| &*v.key))
            .collect();
        assert_eq!(under, vec!["cs.RO"], "a mint during the sort survives it");
        assert!(live.rebuild_wanted(), "and the next rebuild is still owed");
    }
}
