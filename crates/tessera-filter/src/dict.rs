//! A layer's sorted dictionary of distinct keys: front-coded blocks with periodic restarts, the
//! writer that appends them and the fail-closed reader that searches them
//! (`records-and-search.md` §4.3).
//!
//! **Built, with all four consumers.** The base build and flush write this file, the read route
//! resolves needles against it, the coalesce merges two under the content guard §7 requires, and
//! the fold rebuilds one (records §7). This crate's own tests still stand alone from those
//! consumers — they are the format's, not the family's.
//!
//! # Never served, and the ordinals are not durable
//!
//! **The dictionary is an index internal.** No key and no ordinal crosses the trust boundary
//! (**I10**): a keyword has no value set, no `visibility`, no `/v1/categories` counterpart and no
//! autocomplete, and records §4.3 refuses each by name. A client that could enumerate keys would be
//! reading a corpus-wide, pre-mask fact — which is what makes this file's audience the scan, and
//! only the scan.
//!
//! **An ordinal is a position in *this* dictionary and means nothing anywhere else.** It is minted
//! at flush, renumbered at coalesce and rebuilt from nothing at every fold. That churn is not a
//! cost the design tolerates — it is the point. A category's code is pinned in the vocabulary and
//! therefore carries a reuse hazard the schema has to police; a keyword's ordinal is manufactured
//! per layer, so no identity survives to be reused and none of that machinery is owed. The rule the
//! consumers inherit: **resolving one layer's ordinals against another layer's dictionary is a
//! recolouring with no symptom**, which is why a layer's index files are one atomic manifest unit
//! (records §7, review B2) and why nothing here caches an ordinal across layers.
//!
//! The renumbering is also why `tessera-authz`'s dictionary-extent merge is not a template for the
//! coalesce that will consume this reader. That merge is ordinal-*preserving* by construction, so
//! it needs no content guard; this family's is not, and records §7 states the guard it owes
//! instead — the remap must be monotone, and `merged[remap[i]] == input[i]` verified for every
//! input key before an ordinal is written. Borrowing the preserving merge's assumptions is the
//! review finding that argument exists to prevent.
//!
//! # The format
//!
//! ```text
//! file     := MAGIC | blocks | restarts | footer
//! blocks   := block*                       -- block b holds ordinals [b*K, min((b+1)*K, n))
//! block    := entry+                       -- the first entry's `shared` is 0
//! entry    := shared varint | suffix_len varint | suffix[suffix_len]
//! restarts := u64 LE * (block_count + 1)   -- block starts, relative to `blocks`; last = |blocks|
//! footer   := version u32 | K u32 | key_count u32 | block_count u32 | blocks_len u64 | MAGIC
//! ```
//!
//! A key is `previous_key[..shared] ++ suffix`. `K` is the restart interval, fixed for the file, so
//! **a block's ordinal range is arithmetic rather than stored**: `key_of` divides, and the restart
//! table carries offsets alone. A variable block — one cut to a byte target — would have to store a
//! first ordinal per block as well, buying nothing this family needs, since decode cost is bounded
//! by `K` entries either way.
//!
//! **A block's first key is a contiguous slice of the file**, because its `shared` is 0. That is
//! what lets the binary search over blocks compare keys without decoding or allocating anything;
//! only the sequential walk inside the chosen block materialises a key.
//!
//! The counts sit in a **footer** rather than a header so the writer can stream: block bytes go out
//! as they are coded, and `key_count`, `block_count` and `blocks_len` are known only at the end.
//! The alternative — a header patched by seeking back — would force `Write + Seek` on every
//! producer, and buffering the blocks would put a base dictionary's gigabytes in memory during the
//! fold. What the writer does hold is the restart table, 8 bytes per block: at `K` = 16 that is
//! 0.5 B per key of writer memory, which the fold's streaming envelope (index §6.2) absorbs and a
//! flush never notices.
//!
//! Restart offsets are `u64`. A `u32` would cap the blocks region at 4 GiB, and records §4.3 sizes
//! this family's `contains` route at 10⁹ unique keys — a dictionary past that cap at any plausible
//! bytes-per-key. The cost is measured, not assumed: see the campaign named at
//! [`DEFAULT_RESTART_INTERVAL`].
//!
//! **Keys are compared as bytes, and that is the whole ordering.** UTF-8's byte order agrees with
//! its code-point order, so Rust's `str` comparison — which is byte-wise — is the same relation the
//! file is sorted in, and a prefix's ordinal range is contiguous under both readings. There is no
//! normalisation and no case folding: records §4.3 keeps `utf8`'s byte-exact semantics, so NFC and
//! NFD spellings of the same word are two distinct keys, exactly as they are two distinct values in
//! the column this replaces. Front coding elides shared **bytes**, which may split a multi-byte
//! code point; only the reassembled key is validated as UTF-8, and it is validated on every decode.
//!
//! # The five operations, and what they cost
//!
//! [`SortedDict::resolve`] and [`SortedDict::prefix_range`] binary-search the restart keys and then
//! decode sequentially inside one block: `O(log block_count)` slice comparisons plus at most `K`
//! decodes. [`SortedDict::key_of`] needs no search at all — the ordinal names its block — and
//! decodes at most `K` entries into a caller-owned scratch buffer.
//! [`SortedDict::walk`] decodes every key in order and is the broad `contains` route's whole
//! access pattern. [`SortedDict::walk_ordinals`] is the narrow route's: given ascending ordinals it
//! decodes each block holding one exactly once, which is `key_of` per ordinal with the `K/2`
//! discarded decodes per probe removed — the difference between paying per candidate *entity* and
//! paying per *block the candidate's values occupy*.
//!
//! **`walk` has no early exit, deliberately.** Front coding elides shared prefixes, so a substring
//! can span an elided prefix and every key must be decoded and searched — records §4.3 says so —
//! and the absence of an exit is also what keeps the broad route's work a function of
//! `(candidate, column)` and never of the needle. `walk_ordinals` stops at the last wanted ordinal
//! in each block, which is the same property: what it reads is fixed by the ordinals it was handed,
//! and those come from the candidate rather than from the needle. The lookups are not constant-time
//! in the same
//! way: `resolve` decodes a needle-dependent number of entries within its block, a handful of
//! nanoseconds either way. That does not open the channel per-point-attributes §3.8 closes, because
//! §4.3's rule is that **an unresolved needle still scans** — the millisecond-scale work that
//! follows is identical whether the needle resolved or not, and a lookup that shaved nanoseconds
//! off a miss would still be followed by the same scan.
//!
//! # The read is fail-closed
//!
//! A dictionary that answered wrongly would mis-colour a whole layer's values, so every decode is
//! total: `shared` may not exceed the previous key's length, an entry may not run past its block,
//! a block must tile its extent exactly, a restart offset must lie inside the blocks region, a
//! block's first entry must restart, every key must be valid UTF-8, and **every key must be
//! strictly greater than the one before it**. Any violation is a [`DictError::Malformed`] refusal;
//! none of them can produce a neighbour's key.
//!
//! **Inside a block the order check costs no copy**, because front coding makes it local: comparing
//! a key with its predecessor needs only the predecessor's length and its byte at position
//! `shared`, both still in the decode buffer at the moment the new suffix is appended. So
//! `key > prev` is exactly `suffix_len >= 1` when `shared == prev.len()`, and
//! `suffix[0] > prev[shared]` when `shared < prev.len()`. Two consequences worth naming. First,
//! **`suffix_len` is never zero**: a zero-length suffix is either a duplicate key or a strict
//! prefix of its predecessor, and both are out of order. Second, refusing on *equality* at byte
//! `shared` pins `shared` to the exact common prefix rather than merely a legal one, so **the
//! encoding is canonical** — one byte sequence per key list per restart interval — which is what
//! makes the fold's byte-level equality argument (records §7) checkable rather than aspirational.
//!
//! **Across a restart the check is a whole-key comparison, and it has to be.** A restart's `shared`
//! is 0 by construction, not because the keys have nothing in common, so the local shortcut has
//! nothing to stand on: two blocks whose first keys legitimately share a prefix would fail a
//! byte-0 comparison. The buffer is still threaded through the whole file by
//! [`SortedDict::walk`] — the previous block's last key is what the new one is compared against —
//! but the comparison there is over both keys entire, once per block.
//!
//! What a single read cannot see is the part of the artefact it does not touch: a binary search
//! reads `log(block_count)` restart keys and trusts the rest to be ordered. [`SortedDict::resolve`]
//! is unharmed either way — it verifies the key it lands on against the needle, so it can return a
//! false miss under corruption but never a wrong ordinal — and [`SortedDict::prefix_range`], which
//! has no such comparison available, checks the four keys bracketing the range it is about to
//! return. [`SortedDict::self_check`] walks the entire file, and since the decoder's per-entry
//! checks are total the walk *is* the whole-artefact check; it is what the conformance suite runs,
//! addressing being invisible from the served surface.
//!
//! `values.rs` and `record.rs` each carry a second `unsafe` block beyond the mapping call, to hand
//! Arrow a `Buffer` built from the mapping's raw pointer, and each owes a lifetime argument for it.
//! This module owes none: the format is byte-addressed, so a mapping that derefs to `[u8]` is the
//! whole of what the reader wants, and the only `unsafe` left is `Mmap::map` itself.
//!
//! # Why front-coded blocks and not an FST
//!
//! An FST would serve, and `probes/2026-08-03-dict-fst/` measured one for the authorisation
//! dictionary. Front-coded blocks are chosen because the byte target is met with the restart
//! overhead counted, the build is an append over sorted keys rather than an automaton
//! construction — which is what lets a flush code its batch on the pool at flush execution
//! (records §7) — and nothing this family does needs infix sharing. Revisit only with a
//! measurement.

use std::fmt;
use std::io::{self, BufWriter, Write};
use std::ops::{Deref, Range};
use std::path::Path;

use crate::values::Access;

/// The layer's dictionary file, under `attrs/<column>/` (records §7). One file per layer — an
/// extent's ordinals are meaningful only against that extent's own dictionary.
pub const DICT_FILE: &str = "dict.bin";

/// Bumped whenever the byte layout changes, and checked exactly at open. Pre-release there is no
/// past to be compatible with (decision 0048); the version exists so a stale local artefact refuses
/// loudly instead of being misread.
pub const DICT_FORMAT_VERSION: u32 = 1;

/// Keys per block: the restart interval a writer uses unless told otherwise.
///
/// **Chosen by measurement**, over the real arXiv columns records §4.3 quotes, in
/// `probes/2026-08-13-keyword-dict/`. The trade is a restart offset and one un-elided key per
/// block against `K/2` decodes per lookup. Doubling from 16 to 32 saves 0.50–0.95 B/key — 2% of
/// the `submitter` layout, 6% of `id`'s, 9% of `doi`'s — and costs 31% on [`SortedDict::key_of`],
/// which the narrow `contains` route pays **per candidate entity** while the bytes are paid once.
/// 16 also puts that probe at 0.10 µs, the bottom of §4.3's modelled 0.1–0.3 µs band rather than
/// its middle. The campaign's table is there for a reader who weighs the two differently.
pub const DEFAULT_RESTART_INTERVAL: u32 = 16;

/// `TSDC` — Tessera sorted dictionary. Written at both ends of the file, so a truncated tail is
/// refused by the footer's copy before any offset in it is believed.
const MAGIC: [u8; 4] = *b"TSDC";

/// `version | restart_interval | key_count | block_count | blocks_len | MAGIC`.
const FOOTER_LEN: usize = 4 + 4 + 4 + 4 + 8 + 4;

/// The shortest legal file: leading magic, no blocks, the lone sentinel restart, the footer.
const MIN_FILE_LEN: usize = MAGIC.len() + 8 + FOOTER_LEN;

/// A dictionary read's refusal, typed so a caller can tell an environment failure from a malformed
/// artefact. Both non-`Io` variants are refusals: the read answers nothing rather than answering
/// from a file it cannot vouch for.
#[derive(Debug)]
pub enum DictError {
    /// The file system failed underneath the read or write.
    Io(io::Error),
    /// The artefact is malformed — a short file, a restart offset outside the blocks region, a
    /// block that does not decode or does not tile its extent, keys out of order, a key that is not
    /// UTF-8 — or a writer was handed keys that are not strictly ascending.
    Malformed(String),
    /// An ordinal past the end of this dictionary. Distinct from [`DictError::Malformed`] because
    /// its usual cause is a *pair* that disagrees — an ordinal column read against the wrong
    /// layer's dictionary, which records §7 makes one atomic manifest unit precisely to prevent —
    /// rather than damage inside this file.
    OrdinalOutOfRange { ordinal: u32, len: u32 },
}

impl fmt::Display for DictError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            DictError::Io(e) => write!(f, "sorted dictionary: {e}"),
            DictError::Malformed(detail) => write!(f, "sorted dictionary refuses: {detail}"),
            DictError::OrdinalOutOfRange { ordinal, len } => write!(
                f,
                "sorted dictionary refuses: ordinal {ordinal} past the end of a {len}-key dictionary"
            ),
        }
    }
}

impl std::error::Error for DictError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            DictError::Io(e) => Some(e),
            _ => None,
        }
    }
}

impl From<io::Error> for DictError {
    fn from(e: io::Error) -> Self {
        DictError::Io(e)
    }
}

impl From<DictError> for io::Error {
    fn from(e: DictError) -> Self {
        match e {
            DictError::Io(inner) => inner,
            other => io::Error::new(io::ErrorKind::InvalidData, other.to_string()),
        }
    }
}

fn malformed(detail: impl Into<String>) -> DictError {
    DictError::Malformed(detail.into())
}

/// What a completed dictionary came to. Returned by the writer so a producer can record the layer's
/// size without reopening the file, and so a measurement harness can quote bytes per key.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct DictStats {
    pub keys: u32,
    pub blocks: u32,
    pub bytes: u64,
}

// ---------------------------------------------------------------------------------------------
// The writer
// ---------------------------------------------------------------------------------------------

/// Appends **already-sorted, distinct** keys and produces the file.
///
/// The sort is the caller's because every producer already has one: a flush sorts its batch's
/// distinct values on the pool, a coalesce merges two sorted key sets, and a fold streams a sorted
/// column. Sorting again here would be a second pass over the same keys for no new guarantee.
///
/// **A key out of order or repeated is refused, never repaired.** Silently de-duplicating would
/// shift every subsequent ordinal under a caller that is building the ordinal column against this
/// sequence — the one defect that mis-colours a layer without leaving a symptom — so the writer
/// refuses and the producer stops.
pub struct SortedDictWriter<W: Write> {
    out: W,
    restart_interval: u32,
    /// Block starts relative to the blocks region, plus a sentinel at [`Self::finish`].
    restarts: Vec<u64>,
    /// Bytes written into the blocks region so far.
    blocks_len: u64,
    prev: Vec<u8>,
    keys: u32,
}

impl<W: Write> SortedDictWriter<W> {
    /// Start a dictionary at [`DEFAULT_RESTART_INTERVAL`].
    pub fn new(out: W) -> Result<Self, DictError> {
        Self::with_restart_interval(out, DEFAULT_RESTART_INTERVAL)
    }

    /// Start a dictionary at an explicit restart interval. Exists for the measurement campaign that
    /// chose the default; a producer should take the default.
    pub fn with_restart_interval(mut out: W, restart_interval: u32) -> Result<Self, DictError> {
        if restart_interval == 0 {
            return Err(malformed("a restart interval of 0 names no block"));
        }
        out.write_all(&MAGIC)?;
        Ok(SortedDictWriter {
            out,
            restart_interval,
            restarts: Vec::new(),
            blocks_len: 0,
            prev: Vec::new(),
            keys: 0,
        })
    }

    /// Append `key`, returning the ordinal it takes.
    ///
    /// Refuses the empty string — the ingest wire refuses it as a value (records §7), so an empty
    /// key could only be a defect — a key not strictly greater than its predecessor, and a
    /// dictionary past `u32::MAX` keys, which an ordinal could not name.
    pub fn push(&mut self, key: &str) -> Result<u32, DictError> {
        let bytes = key.as_bytes();
        if bytes.is_empty() {
            return Err(malformed(format!(
                "the empty string is not a key (at ordinal {})",
                self.keys
            )));
        }
        if self.keys > 0 && bytes <= self.prev.as_slice() {
            return Err(malformed(format!(
                "keys must ascend strictly: {:?} follows {:?} at ordinal {}",
                key,
                String::from_utf8_lossy(&self.prev),
                self.keys
            )));
        }
        if self.keys == u32::MAX {
            return Err(malformed(
                "a dictionary cannot hold more than u32::MAX keys",
            ));
        }

        let restarts_here = self.keys.is_multiple_of(self.restart_interval);
        if restarts_here {
            self.restarts.push(self.blocks_len);
        }
        let shared = if restarts_here {
            0
        } else {
            common_prefix_len(&self.prev, bytes)
        };
        let suffix = &bytes[shared..];

        write_varint(&mut self.out, shared as u32)?;
        write_varint(&mut self.out, suffix.len() as u32)?;
        self.out.write_all(suffix)?;
        self.blocks_len +=
            (varint_len(shared as u32) + varint_len(suffix.len() as u32) + suffix.len()) as u64;

        self.prev.clear();
        self.prev.extend_from_slice(bytes);
        let ordinal = self.keys;
        self.keys += 1;
        Ok(ordinal)
    }

    /// Write the restart table and the footer, and return what the file came to.
    pub fn finish(mut self) -> Result<DictStats, DictError> {
        let blocks = self.restarts.len() as u32;
        // The sentinel: every block's extent is `restarts[b]..restarts[b + 1]`, so the last block
        // needs a successor. Carrying it rather than deriving it from `blocks_len` is redundancy
        // the reader checks — the same reason `record.rs`'s directory carries lengths it could
        // compute.
        self.restarts.push(self.blocks_len);
        for offset in &self.restarts {
            self.out.write_all(&offset.to_le_bytes())?;
        }
        self.out.write_all(&DICT_FORMAT_VERSION.to_le_bytes())?;
        self.out.write_all(&self.restart_interval.to_le_bytes())?;
        self.out.write_all(&self.keys.to_le_bytes())?;
        self.out.write_all(&blocks.to_le_bytes())?;
        self.out.write_all(&self.blocks_len.to_le_bytes())?;
        self.out.write_all(&MAGIC)?;
        self.out.flush()?;
        Ok(DictStats {
            keys: self.keys,
            blocks,
            bytes: (MAGIC.len() + FOOTER_LEN) as u64
                + self.blocks_len
                + self.restarts.len() as u64 * 8,
        })
    }
}

/// Write a dictionary at `path` over already-sorted distinct `keys`.
pub fn write_sorted_dict<'a, I>(path: &Path, keys: I) -> Result<DictStats, DictError>
where
    I: IntoIterator<Item = &'a str>,
{
    let mut writer = SortedDictWriter::new(BufWriter::new(std::fs::File::create(path)?))?;
    for key in keys {
        writer.push(key)?;
    }
    writer.finish()
}

fn common_prefix_len(a: &[u8], b: &[u8]) -> usize {
    a.iter().zip(b).take_while(|(x, y)| x == y).count()
}

fn varint_len(v: u32) -> usize {
    match v {
        0..=0x7f => 1,
        0x80..=0x3fff => 2,
        0x4000..=0x1f_ffff => 3,
        0x20_0000..=0x0fff_ffff => 4,
        _ => 5,
    }
}

fn write_varint(out: &mut impl Write, mut v: u32) -> io::Result<()> {
    let mut buf = [0u8; 5];
    let mut n = 0;
    loop {
        let byte = (v & 0x7f) as u8;
        v >>= 7;
        if v == 0 {
            buf[n] = byte;
            n += 1;
            break;
        }
        buf[n] = byte | 0x80;
        n += 1;
    }
    out.write_all(&buf[..n])
}

// ---------------------------------------------------------------------------------------------
// The reader
// ---------------------------------------------------------------------------------------------

/// The file's bytes, mapped or owned.
///
/// The same choice `values.rs` and `record.rs` make, without their `unsafe`: those two hand the
/// bytes to Arrow as a zero-copy `Buffer` and owe a safety argument for the mapping's lifetime,
/// while a byte-addressed format needs nothing but a slice.
#[derive(Debug)]
enum DictBytes {
    Owned(Vec<u8>),
    Mapped(memmap2::Mmap),
}

impl Deref for DictBytes {
    type Target = [u8];
    fn deref(&self) -> &[u8] {
        match self {
            DictBytes::Owned(v) => v,
            DictBytes::Mapped(m) => m,
        }
    }
}

/// A needle prepared once for the many keys a `contains` route tests it against.
///
/// **The preparation is the point.** Front coding makes a substring search a per-key loop rather
/// than a scan of the file's bytes — a match can span an elided prefix — so the broad `contains`
/// route runs one search per dictionary key and the narrow route one per candidate entity. Written
/// as `key.contains(needle)`, each of those constructs a two-way searcher from the needle and
/// discards it, and at a few tens of bytes per key the construction is most of the work:
/// `2026-08-13-contains-recovery` measures the shipped walk at 19.91–44.09 ns per key against
/// 15.42–26.57 with the searcher hoisted, and shows the shipped cost climbing with the needle's
/// length (`doi`: 19.91 → 39.06 ns from a 3-byte needle to a 16-byte one) where the hoisted cost
/// stays flat (17.66 → 18.84). That climb is what the retirement fence saw and could not explain.
///
/// The test itself is unconditional, so hoisting changes no work the route does per key: a
/// dictionary walk still reads every key whatever the needle is.
pub struct KeyMatcher<'n> {
    finder: memchr::memmem::Finder<'n>,
}

impl<'n> KeyMatcher<'n> {
    /// Prepare `needle`. An empty needle matches every key, which is the `contains` contract's own
    /// reading and what `Finder` already answers.
    pub fn new(needle: &'n str) -> Self {
        KeyMatcher {
            finder: memchr::memmem::Finder::new(needle.as_bytes()),
        }
    }

    /// Whether `key` contains the needle — the same answer as `str::contains`, asserted by
    /// `agrees_with_str_contains`.
    pub fn matches(&self, key: &str) -> bool {
        self.finder.find(key.as_bytes()).is_some()
    }
}

impl fmt::Debug for KeyMatcher<'_> {
    /// The needle is a principal's own query text. It is not printed, so a `Debug` of anything
    /// holding a matcher cannot put it in a log.
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("KeyMatcher(..)")
    }
}

/// One layer's front-coded sorted dictionary, opened for reading.
///
/// Open validates what it can in constant time — both magics, the version, that the file's length
/// is exactly what the footer's counts imply, and that the block count is the one the key count and
/// the restart interval force. It deliberately does **not** sweep the restart table: at 10⁹ keys
/// that table is hundreds of megabytes, a generation opens every declared column at once, and the
/// sweep would buy nothing a per-use bounds check does not already give. Every offset is checked
/// where it is used, and [`Self::self_check`] is the exhaustive pass.
#[derive(Debug)]
pub struct SortedDict {
    bytes: DictBytes,
    key_count: u32,
    block_count: u32,
    restart_interval: u32,
    /// Byte offset of the restart table within the file.
    restarts_at: usize,
}

impl SortedDict {
    /// Open the dictionary in `dir` — `attrs/<column>/` — by its canonical file name.
    pub fn open_dir(dir: &Path, access: Access) -> Result<Self, DictError> {
        Self::open(&dir.join(DICT_FILE), access)
    }

    /// Open a dictionary from an explicit path (an extent's file carries a flush-scoped name).
    ///
    /// The manifest's digest is the outer guard over these bytes; the checks here are the format's
    /// own, which a digest cannot express.
    pub fn open(path: &Path, access: Access) -> Result<Self, DictError> {
        let len = std::fs::metadata(path)?.len();
        if (len as usize) < MIN_FILE_LEN {
            return Err(malformed(format!(
                "{} is {len} bytes, shorter than the {MIN_FILE_LEN}-byte minimum",
                path.display()
            )));
        }
        let bytes = match access {
            Access::Read => DictBytes::Owned(std::fs::read(path)?),
            Access::Mapped | Access::MappedSequential => {
                let file = std::fs::File::open(path)?;
                // SAFETY: the same argument every mapped artefact here makes — a bundle file is not
                // written under a reader, so the mapping's bytes do not change beneath it. `Mmap`
                // owns its mapping and derefs to `[u8]`, so nothing further is asserted: unlike
                // `values.rs` and `record.rs`, no raw pointer is handed to Arrow.
                let mapping = unsafe { memmap2::Mmap::map(&file) }?;
                if access == Access::MappedSequential {
                    // A hint, and a failure to give it is not a failure to open (decision 0052):
                    // the fold streams a dictionary once and wants the drop-behind; a kernel that
                    // declines still leaves a correct mapping.
                    let _ = mapping.advise(memmap2::Advice::Sequential);
                }
                DictBytes::Mapped(mapping)
            }
        };
        Self::from_bytes(bytes, &path.display().to_string())
    }

    /// Open a dictionary held in memory — the writer's read-back, and a producer that codes a small
    /// extent without going through the file system.
    pub fn from_vec(bytes: Vec<u8>) -> Result<Self, DictError> {
        Self::from_bytes(DictBytes::Owned(bytes), "an in-memory dictionary")
    }

    fn from_bytes(bytes: DictBytes, origin: &str) -> Result<Self, DictError> {
        let len = bytes.len();
        if len < MIN_FILE_LEN {
            return Err(malformed(format!(
                "{origin} is {len} bytes, shorter than the {MIN_FILE_LEN}-byte minimum"
            )));
        }
        if bytes[..MAGIC.len()] != MAGIC {
            return Err(malformed(format!("{origin} does not start with 'TSDC'")));
        }
        let footer = len - FOOTER_LEN;
        if bytes[len - MAGIC.len()..] != MAGIC {
            return Err(malformed(format!(
                "{origin} does not end with 'TSDC' — a truncated or overwritten tail"
            )));
        }
        let version = read_u32_at(&bytes, footer);
        if version != DICT_FORMAT_VERSION {
            return Err(malformed(format!(
                "{origin} is format version {version}, and this reader is version {DICT_FORMAT_VERSION}"
            )));
        }
        let restart_interval = read_u32_at(&bytes, footer + 4);
        let key_count = read_u32_at(&bytes, footer + 8);
        let block_count = read_u32_at(&bytes, footer + 12);
        let blocks_len = read_u64_at(&bytes, footer + 16);

        if restart_interval == 0 {
            return Err(malformed(format!(
                "{origin} claims a restart interval of 0"
            )));
        }
        // Bound the footer's own claim before it is used in arithmetic: `blocks_len` is read from
        // the file, and a doctored one would otherwise overflow the length reconciliation below
        // rather than fail it.
        if blocks_len > len as u64 {
            return Err(malformed(format!(
                "{origin} claims a {blocks_len}-byte blocks region in a {len}-byte file"
            )));
        }
        let expected_blocks = key_count.div_ceil(restart_interval);
        if block_count != expected_blocks {
            return Err(malformed(format!(
                "{origin} claims {block_count} blocks where {key_count} keys at a restart interval \
                 of {restart_interval} force {expected_blocks}"
            )));
        }
        // The whole file is accounted for, exactly: magic, blocks, one restart per block plus the
        // sentinel, footer. This is the check a truncation fails.
        let restart_bytes = (block_count as u64 + 1) * 8;
        let expected_len = MAGIC.len() as u64 + blocks_len + restart_bytes + FOOTER_LEN as u64;
        if expected_len != len as u64 {
            return Err(malformed(format!(
                "{origin} is {len} bytes where its footer's counts imply {expected_len}"
            )));
        }
        let restarts_at = MAGIC.len() + blocks_len as usize;
        // The sentinel and the first offset are the two the arithmetic above cannot force.
        let dict = SortedDict {
            bytes,
            key_count,
            block_count,
            restart_interval,
            restarts_at,
        };
        if block_count > 0 {
            let first = dict.restart_offset(0);
            let sentinel = dict.restart_offset(block_count);
            if first != 0 || sentinel != blocks_len {
                return Err(malformed(format!(
                    "{origin}'s restart table runs {first}..{sentinel} where the blocks region is \
                     0..{blocks_len}"
                )));
            }
        } else if key_count != 0 || blocks_len != 0 || dict.restart_offset(0) != 0 {
            return Err(malformed(format!(
                "{origin} has no blocks but claims {key_count} keys in {blocks_len} bytes"
            )));
        }
        Ok(dict)
    }

    /// How many keys this dictionary holds. Ordinals run `0..len()`.
    pub fn len(&self) -> u32 {
        self.key_count
    }

    pub fn is_empty(&self) -> bool {
        self.key_count == 0
    }

    /// Keys per block, as this file was written.
    pub fn restart_interval(&self) -> u32 {
        self.restart_interval
    }

    /// Blocks in this file — `len()` divided by [`Self::restart_interval`], rounded up.
    pub fn block_count(&self) -> u32 {
        self.block_count
    }

    /// The ordinal of `needle`, or `None` if this layer does not hold it.
    ///
    /// **A miss is ordinary and is not an error.** records §4.3's rule is that an unresolved needle
    /// still scans: skipping the scan would make a value that exists somewhere in the corpus
    /// distinguishable *in work* from one that does not. The `Result` is for a malformed file,
    /// which must refuse rather than answer `None` — a corrupt dictionary that reported misses
    /// would blank a filter silently.
    pub fn resolve(&self, needle: &str) -> Result<Option<u32>, DictError> {
        let (ordinal, exact) = self.lower_bound(needle.as_bytes())?;
        Ok(exact.then_some(ordinal))
    }

    /// The contiguous ordinal range whose keys start with `prefix`; empty when none do.
    ///
    /// Contiguity is the reason the dictionary is sorted at all: the scan then tests an ordinal
    /// *range*, which is the numeric-range comparison the fixed-width scan already runs (§4.3).
    /// An empty prefix returns the whole dictionary, which is the honest reading — every key starts
    /// with it.
    ///
    /// The four keys bracketing the answer are verified before it is returned. `resolve` can check
    /// itself against the needle it was given; a range has no such comparison available, so it
    /// makes one: the keys just inside the range must carry the prefix and the keys just outside it
    /// must not. It costs at most four block decodes on an operation that runs once per request.
    pub fn prefix_range(&self, prefix: &str) -> Result<Range<u32>, DictError> {
        let lo = prefix.as_bytes();
        let start = self.lower_bound(lo)?.0;
        let end = match prefix_upper_bound(lo) {
            Some(hi) => self.lower_bound(&hi)?.0,
            // Every byte of the prefix is 0xFF, so nothing sorts above it. Unreachable from valid
            // UTF-8, which never contains 0xFF, and three lines cheaper than an argument that it is.
            None => self.key_count,
        };
        if start > end {
            return Err(malformed(format!(
                "prefix {prefix:?} bounds the range {start}..{end}, which runs backwards"
            )));
        }
        let mut scratch = Vec::new();
        let mut carries = |ordinal: u32, want: bool| -> Result<(), DictError> {
            let key = self.key_of(ordinal, &mut scratch)?;
            if key.as_bytes().starts_with(lo) != want {
                return Err(malformed(format!(
                    "prefix {prefix:?} bounds {start}..{end}, but the key at ordinal {ordinal} \
                     {} it",
                    if want { "does not carry" } else { "carries" }
                )));
            }
            Ok(())
        };
        if start > 0 {
            carries(start - 1, false)?;
        }
        if start < end {
            carries(start, true)?;
            carries(end - 1, true)?;
        }
        if end < self.key_count {
            carries(end, false)?;
        }
        Ok(start..end)
    }

    /// The key at `ordinal`, decoded into `scratch`.
    ///
    /// The scratch buffer is the caller's so the narrow `contains` route — one dictionary probe per
    /// candidate entity — pays no allocation per probe. Its contents on entry are discarded; its
    /// capacity is reused.
    pub fn key_of<'s>(&self, ordinal: u32, scratch: &'s mut Vec<u8>) -> Result<&'s str, DictError> {
        if ordinal >= self.key_count {
            return Err(DictError::OrdinalOutOfRange {
                ordinal,
                len: self.key_count,
            });
        }
        let block = ordinal / self.restart_interval;
        let want = ordinal;
        scratch.clear();
        let mut found = false;
        self.decode_block(block, scratch, &mut |o, _| {
            found = o == want;
            !found
        })?;
        if !found {
            return Err(malformed(format!(
                "ordinal {ordinal} is not in block {block}, which the restart interval places it in"
            )));
        }
        std::str::from_utf8(scratch).map_err(|e| {
            malformed(format!(
                "the key at ordinal {ordinal} is not valid UTF-8: {e}"
            ))
        })
    }

    /// Visit every key in ordinal order, exactly once.
    ///
    /// **There is no early exit, and that is deliberate.** The broad `contains` route decodes and
    /// searches every key — a substring can span an elided prefix, so a flat scan of the file's
    /// bytes would miss matches (records §4.3, review N5) — and reading the whole dictionary
    /// whatever the needle is also what keeps that route's work a function of `(candidate, column)`
    /// alone. A caller that wants to stop early wants a different operation.
    ///
    /// One decode buffer is threaded through the whole file so the order check spans block
    /// boundaries.
    pub fn walk(&self, mut f: impl FnMut(u32, &str)) -> Result<(), DictError> {
        let mut key = Vec::new();
        let mut error: Option<DictError> = None;
        for block in 0..self.block_count {
            self.decode_block(
                block,
                &mut key,
                &mut |ordinal, bytes| match std::str::from_utf8(bytes) {
                    Ok(s) => {
                        f(ordinal, s);
                        true
                    }
                    Err(e) => {
                        error = Some(malformed(format!(
                            "the key at ordinal {ordinal} is not valid UTF-8: {e}"
                        )));
                        false
                    }
                },
            )?;
            if let Some(e) = error.take() {
                return Err(e);
            }
        }
        Ok(())
    }

    /// Visit the keys at `wanted` — **strictly ascending ordinals** — decoding each block that
    /// holds one exactly once.
    ///
    /// **This is [`Self::key_of`] amortised, and the amortisation is the point.** A probe decodes
    /// its ordinal's whole block prefix and returns one key, so `restart_interval / 2` decodes are
    /// discarded per probe on average; the narrow `contains` route paid that per *candidate
    /// entity*. Given the candidate's ordinals sorted and deduplicated, this decodes each needed
    /// block once and emits every wanted key in it —
    /// `2026-08-13-contains-recovery` measures the whole replacement — this plus deduplication
    /// plus a hoisted searcher — at **1.91–6.05×** the probe-per-entity loop across six real
    /// shapes, best on a contiguous candidate and worst on a scattered one, where the candidate
    /// touches nearly every block and this degenerates to [`Self::walk`].
    ///
    /// Refusing a list that does not ascend strictly is not fastidiousness: the block grouping and
    /// the per-block cursor both assume it, and a duplicate or a step backwards would silently skip
    /// keys rather than answer wrongly in a way the caller could see.
    ///
    /// Like `key_of` and unlike [`Self::walk`], this checks ordering *within* the blocks it opens
    /// and across the ones it happens to visit in sequence — never across the blocks it skips. The
    /// exhaustive pass is still [`Self::self_check`].
    pub fn walk_ordinals(
        &self,
        wanted: &[u32],
        mut f: impl FnMut(u32, &str),
    ) -> Result<(), DictError> {
        let mut key = Vec::new();
        let mut error: Option<DictError> = None;
        let mut i = 0usize;
        while i < wanted.len() {
            let first = wanted[i];
            if first >= self.key_count {
                return Err(DictError::OrdinalOutOfRange {
                    ordinal: first,
                    len: self.key_count,
                });
            }
            let block = first / self.restart_interval;
            // The run of wanted ordinals landing in this block, checking the caller's ordering as
            // it goes — every adjacent pair is compared exactly once, here or at the next block.
            let mut j = i + 1;
            while j < wanted.len() {
                if wanted[j] <= wanted[j - 1] {
                    return Err(malformed(format!(
                        "ordinal {} follows {} in the requested list, which must ascend strictly",
                        wanted[j],
                        wanted[j - 1]
                    )));
                }
                if wanted[j] / self.restart_interval != block {
                    break;
                }
                j += 1;
            }
            let last = wanted[j - 1];
            let mut at = i;
            self.decode_block(block, &mut key, &mut |ordinal, bytes| {
                if at < j && ordinal == wanted[at] {
                    match std::str::from_utf8(bytes) {
                        Ok(s) => {
                            f(ordinal, s);
                            at += 1;
                        }
                        Err(e) => {
                            error = Some(malformed(format!(
                                "the key at ordinal {ordinal} is not valid UTF-8: {e}"
                            )));
                            return false;
                        }
                    }
                }
                // Stop at the last wanted ordinal in this block rather than decoding its tail.
                ordinal < last
            })?;
            if let Some(e) = error.take() {
                return Err(e);
            }
            if at != j {
                return Err(malformed(format!(
                    "ordinal {} is not in block {block}, which the restart interval places it in",
                    wanted[at]
                )));
            }
            i = j;
        }
        Ok(())
    }

    /// Walk the whole artefact, refusing at the first defect.
    ///
    /// The decoder's per-entry checks are total — extents, tiling, shared bounds, ordering, UTF-8 —
    /// so the walk *is* the exhaustive check, and this is the conformance suite's entry point:
    /// addressing is invisible from the served surface, so nothing above can see a dictionary that
    /// decodes wrongly.
    pub fn self_check(&self) -> Result<(), DictError> {
        self.walk(|_, _| {})
    }

    /// The `b`-th restart offset. Callers must have bounds-checked `b <= block_count`.
    fn restart_offset(&self, b: u32) -> u64 {
        read_u64_at(&self.bytes, self.restarts_at + b as usize * 8)
    }

    /// Block `b`'s absolute byte extent in the file, bounds-checked against the blocks region.
    fn block_extent(&self, b: u32) -> Result<(usize, usize), DictError> {
        if b >= self.block_count {
            return Err(malformed(format!(
                "block {b} is past the {}-block dictionary",
                self.block_count
            )));
        }
        let blocks_len = (self.restarts_at - MAGIC.len()) as u64;
        let start = self.restart_offset(b);
        let end = self.restart_offset(b + 1);
        if start >= end || end > blocks_len {
            return Err(malformed(format!(
                "block {b}'s restart offsets are {start}..{end}, outside the 0..{blocks_len} blocks \
                 region or empty"
            )));
        }
        Ok((MAGIC.len() + start as usize, MAGIC.len() + end as usize))
    }

    /// How many keys block `b` holds. A block is full at the restart interval except the last.
    fn block_keys(&self, b: u32) -> u32 {
        let first = b as u64 * self.restart_interval as u64;
        (self.key_count as u64 - first).min(self.restart_interval as u64) as u32
    }

    /// Decode block `b`, appending each key into `key` and passing `(ordinal, key)` to `f` until it
    /// returns `false` or the block is exhausted.
    ///
    /// `key` carries the previous key in and the last decoded key out. Passing an empty buffer is
    /// how a random access starts a block cold; passing the buffer the previous block left is what
    /// extends the ordering check across the boundary at no cost.
    fn decode_block(
        &self,
        b: u32,
        key: &mut Vec<u8>,
        f: &mut impl FnMut(u32, &[u8]) -> bool,
    ) -> Result<(), DictError> {
        let (start, end) = self.block_extent(b)?;
        let keys = self.block_keys(b);
        let first_ordinal = b * self.restart_interval;
        let bytes: &[u8] = &self.bytes;
        let mut at = start;
        let mut stopped = false;

        for i in 0..keys {
            let ordinal = first_ordinal + i;
            let shared = read_varint(bytes, &mut at, end).ok_or_else(|| {
                malformed(format!(
                    "the shared length of ordinal {ordinal} runs past block {b}"
                ))
            })? as usize;
            let suffix_len = read_varint(bytes, &mut at, end).ok_or_else(|| {
                malformed(format!(
                    "the suffix length of ordinal {ordinal} runs past block {b}"
                ))
            })? as usize;
            if i == 0 && shared != 0 {
                return Err(malformed(format!(
                    "block {b} opens at ordinal {ordinal} with a shared prefix of {shared}, where a \
                     restart shares nothing"
                )));
            }
            if shared > key.len() {
                return Err(malformed(format!(
                    "ordinal {ordinal} shares {shared} bytes with a {}-byte predecessor",
                    key.len()
                )));
            }
            if suffix_len == 0 {
                return Err(malformed(format!(
                    "ordinal {ordinal} has an empty suffix, so it repeats or is a prefix of its \
                     predecessor — keys ascend strictly"
                )));
            }
            if end - at < suffix_len {
                return Err(malformed(format!(
                    "ordinal {ordinal}'s {suffix_len}-byte suffix runs past block {b}"
                )));
            }
            let suffix = &bytes[at..at + suffix_len];
            if i == 0 {
                // A restart shares nothing *by construction*, not because the keys have nothing in
                // common, so the local check below has no ground to stand on here and the whole key
                // is compared instead. One comparison per block.
                if !key.is_empty() && suffix <= key.as_slice() {
                    return Err(malformed(format!(
                        "ordinal {ordinal} opens block {b} without following the previous block's \
                         last key"
                    )));
                }
            } else if shared < key.len() && suffix[0] <= key[shared] {
                // The order check, local because front coding makes it so: with `shared` bytes in
                // common the keys differ first at position `shared`, and the predecessor's byte
                // there is still in the buffer. Refusing on equality is what also pins `shared` to
                // the *exact* common prefix, and so pins the encoding to one byte sequence per key
                // list.
                return Err(malformed(format!(
                    "ordinal {ordinal} does not follow its predecessor: byte {shared} is {} where \
                     the predecessor's is {}",
                    suffix[0], key[shared]
                )));
            }
            at += suffix_len;
            key.truncate(shared);
            key.extend_from_slice(suffix);
            if !f(ordinal, key) {
                stopped = true;
                break;
            }
        }
        // A block must tile its extent exactly. Trailing bytes mean the block holds an entry the
        // key count does not account for, which is an addressing defect and not a harmless pad.
        if !stopped && at != end {
            return Err(malformed(format!(
                "block {b} leaves {} bytes after its {keys} keys",
                end - at
            )));
        }
        Ok(())
    }

    /// Block `b`'s first key, as a slice of the file. Zero-copy: a restart shares nothing, so its
    /// suffix *is* the key. This is what the binary search compares.
    fn first_key_of_block(&self, b: u32) -> Result<&[u8], DictError> {
        let (start, end) = self.block_extent(b)?;
        let bytes: &[u8] = &self.bytes;
        let mut at = start;
        let shared = read_varint(bytes, &mut at, end)
            .ok_or_else(|| malformed(format!("block {b}'s first shared length runs past it")))?;
        let suffix_len = read_varint(bytes, &mut at, end)
            .ok_or_else(|| malformed(format!("block {b}'s first suffix length runs past it")))?
            as usize;
        if shared != 0 {
            return Err(malformed(format!(
                "block {b} opens with a shared prefix of {shared}, where a restart shares nothing"
            )));
        }
        if suffix_len == 0 || end - at < suffix_len {
            return Err(malformed(format!(
                "block {b}'s first key is {suffix_len} bytes, which does not fit its extent"
            )));
        }
        Ok(&bytes[at..at + suffix_len])
    }

    /// The first ordinal whose key is `>= x`, and whether that key equals `x` exactly.
    ///
    /// Two searches: the number of blocks whose first key sorts below `x`, then a sequential decode
    /// inside the one block that can straddle the boundary. Everything before that block is below
    /// `x` and everything after it is at or above, because a block's keys lie between its own first
    /// key and the next block's.
    fn lower_bound(&self, x: &[u8]) -> Result<(u32, bool), DictError> {
        if self.key_count == 0 {
            return Ok((0, false));
        }
        let below = self.blocks_below(x)?;
        if below == 0 {
            // Every key sorts at or above `x`; the answer is ordinal 0.
            return Ok((0, self.first_key_of_block(0)? == x));
        }
        // The straddling block, and — if `x` sorts above everything in it — the restart that
        // follows.
        let block = below - 1;
        let mut key = Vec::new();
        let mut hit: Option<(u32, bool)> = None;
        self.decode_block(block, &mut key, &mut |ordinal, bytes| {
            if bytes >= x {
                hit = Some((ordinal, bytes == x));
                false
            } else {
                true
            }
        })?;
        if let Some(found) = hit {
            return Ok(found);
        }
        if below == self.block_count {
            return Ok((self.key_count, false));
        }
        let ordinal = below * self.restart_interval;
        Ok((ordinal, self.first_key_of_block(below)? == x))
    }

    /// How many blocks have a first key strictly below `x`.
    fn blocks_below(&self, x: &[u8]) -> Result<u32, DictError> {
        let mut lo = 0u32;
        let mut hi = self.block_count;
        while lo < hi {
            let mid = lo + (hi - lo) / 2;
            if self.first_key_of_block(mid)? < x {
                lo = mid + 1;
            } else {
                hi = mid;
            }
        }
        Ok(lo)
    }
}

/// The first byte string that sorts above every string starting with `prefix`, or `None` when no
/// such string exists.
///
/// Increments the last byte below `0xFF` and drops what follows it. The result need not be valid
/// UTF-8 — it is a bound, compared byte-wise against keys that are — which is why the search takes
/// bytes rather than `&str`.
fn prefix_upper_bound(prefix: &[u8]) -> Option<Vec<u8>> {
    let mut bound = prefix.to_vec();
    while let Some(last) = bound.pop() {
        if last != 0xff {
            bound.push(last + 1);
            return Some(bound);
        }
    }
    None
}

fn read_u32_at(bytes: &[u8], at: usize) -> u32 {
    u32::from_le_bytes(bytes[at..at + 4].try_into().expect("a four-byte window"))
}

fn read_u64_at(bytes: &[u8], at: usize) -> u64 {
    u64::from_le_bytes(bytes[at..at + 8].try_into().expect("an eight-byte window"))
}

/// LEB128, refusing at `limit` rather than at the end of the file, so an entry cannot read its
/// neighbour's bytes when its own block is malformed.
fn read_varint(bytes: &[u8], at: &mut usize, limit: usize) -> Option<u32> {
    let mut result: u64 = 0;
    for i in 0..5 {
        if *at >= limit {
            return None;
        }
        let byte = bytes[*at];
        *at += 1;
        result |= ((byte & 0x7f) as u64) << (7 * i);
        if byte & 0x80 == 0 {
            return u32::try_from(result).ok();
        }
    }
    None
}
