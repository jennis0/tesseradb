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
//! # The five structures, and why not one
//!
//! [`tessera_filter::SortedDict`] carries no payload and refuses duplicate keys, and two values can
//! fold to one entry string — `cs.LG` and `CS.lg`, or two titles differing only in case — so the
//! index is five mapped files rather than one:
//!
//! | File | What it holds |
//! |---|---|
//! | `entries.dict` | the **distinct** folded entry strings, sorted; two binary searches turn a prefix into `[lo, hi)` |
//! | `runs.bin` | one `u32` per entry string plus a sentinel, indexing `payloads.bin`; entry `e`'s run is `runs[e]..runs[e + 1]` |
//! | `payloads.bin` | one 12-byte record per (entry string, value) pair, ordered within a run by (kind, key) |
//! | `codes.bin` | one `u32` per dense position: that value's vocabulary code |
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
use rustc_hash::FxHashMap;
use tessera_analyse::{EntryKind, SuggestionField, SuggestionFold};
use tessera_filter::{Access, SortedDict, SortedDictWriter};

/// The on-disc shape of the four side arrays and the entry dictionary, together.
///
/// **A fail-closed guard, not compatibility** (decision 0048): nothing outside this process ever
/// reads these files, and a stale one left by a killed build is refused rather than misread.
pub const SUGGEST_FORMAT_VERSION: u32 = 1;

/// The subdirectory of the engine's cache directory every vocabulary's index lives under.
pub const SUGGEST_DIR: &str = "suggest";

/// One payload record: 12 bytes, `u32`-aligned so that the three fields are read without shifts.
const PAYLOAD_LEN: usize = 12;

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

/// One vocabulary's suggestion index: the five mapped structures and the directory holding them.
#[derive(Debug)]
pub struct SuggestIndex {
    dir: PathBuf,
    entries: SortedDict,
    runs: MappedBytes,
    payloads: MappedBytes,
    codes: MappedBytes,
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
        &self,
        fold: &SuggestionFold,
        vocabularies: &tessera_store::vocabulary::Vocabularies,
        mints: &[(String, String, u32)],
    ) -> SuggestIndexes {
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
        SuggestIndexes { by_vocabulary }
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
