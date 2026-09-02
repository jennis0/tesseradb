//! The **sparse delta postings tier** a flush publishes (§3.1, §5.2): the terms its flushed items
//! carried, and nothing else.
//!
//! ## Why this is not `postings.arrow`
//!
//! [`crate::postings`]'s layout is ordinal-indexed — record *i* is term *i* — which is right for a
//! build, where every term has a posting and the ordinals are dense by construction. A flush's
//! terms are an arbitrary handful of ordinals drawn from the whole dictionary, so the same layout
//! would write a file dense to the **highest ordinal used**: an empty record is one tag byte plus
//! an eight-byte offset, so a tier touching one term at ordinal 10⁶ costs ~9 MB, and the plugin
//! ABI's declared `max_distinct_terms` is 2×10⁸. A tier per 90 s, every one of them read by every
//! fragment build, makes that untenable rather than merely wasteful.
//!
//! So a tier stores its term ids alongside its postings and is looked up by binary search. The
//! **posting encoding is `postings.arrow`'s, byte for byte** — [`crate::encode_posting`], the same
//! tag rule, the same [`PostingRef`] — because the two files differ in how a term is *found*, not
//! in what a posting *is*, and a second encoding would be a second thing to get wrong on the
//! authorisation path.
//!
//! ## Absent is not empty
//!
//! [`DeltaTier::posting`] answers `None` for a term this tier does not carry, exactly as
//! [`crate::PostingsReader::posting`] does for a term beyond its range. A fragment build unions
//! over the session's satisfied terms and skips the misses, so a tier contributes only for the
//! terms it actually holds — never its whole term set, which would hand a viewer entities outside
//! `M_auth` (I2).

use std::collections::BTreeMap;
use std::fs::File;
use std::io::{self, Write};
use std::path::{Path, PathBuf};
use std::ptr::NonNull;
use std::sync::Arc;

use arrow::array::{Array, LargeBinaryArray, LargeBinaryBuilder, UInt32Array};
use arrow::buffer::Buffer;
use arrow::datatypes::{DataType, Field, Schema};
use arrow::ipc::writer::FileWriter;
use arrow::record_batch::RecordBatch;

use tessera_types::TermId;

use crate::postings::{
    decode_single_batch, encode_posting, invalid_data, read_posting, PostingRef,
};

const TERM_COLUMN_NAME: &str = "term_id";
const POSTING_COLUMN_NAME: &str = "posting";

/// Write a sparse tier: `entries` is `(term, sorted entity list)` in **strictly ascending term
/// order**.
///
/// Ascending and distinct is checked, not trusted. The lookup is a binary search over the term
/// column, so an unordered file would answer `None` for terms it holds and a duplicated term would
/// make one of the two records unreachable — either way a posting silently disappears, and on this
/// path a disappeared posting is items a viewer is entitled to simply not being there.
///
/// Each entity list must itself be sorted strictly ascending; [`encode_posting`] enforces that and
/// says why.
pub fn write_delta_tier(
    path: &Path,
    entries: &[(TermId, Vec<u32>)],
    small_term_threshold: u32,
) -> io::Result<()> {
    let raw: Vec<(u32, Vec<u32>)> = entries.iter().map(|(t, e)| (t.raw(), e.clone())).collect();
    write_delta_tier_at(path, &raw, small_term_threshold)
}

/// The same writer, addressed by bare record ordinals — the format core
/// ([`crate::PostingsReader::posting_at`] carries the argument for why both exist).
pub fn write_delta_tier_at(
    path: &Path,
    entries: &[(u32, Vec<u32>)],
    small_term_threshold: u32,
) -> io::Result<()> {
    if !entries.windows(2).all(|w| w[0].0 < w[1].0) {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "write_delta_tier: term ids must be strictly ascending (sorted, no duplicates)",
        ));
    }

    let mut terms: Vec<u32> = Vec::with_capacity(entries.len());
    let mut postings = LargeBinaryBuilder::new();
    for (term, entities) in entries {
        terms.push(*term);
        postings.append_value(encode_posting(
            *term as usize,
            entities,
            small_term_threshold,
        )?);
    }

    write_keyed_array(path, UInt32Array::from(terms), postings.finish())
}

/// The single schema/batch/IPC-writer invocation behind [`write_delta_tier_at`] and
/// [`KeyedPostingsSpool::finish`].
///
/// Byte-identity between the buffered and spooled paths requires this to be literally the same
/// code, not two copies that could drift — the same argument [`crate::PostingsSpool`] and
/// `write_posting_records` share `write_posting_array` under. Neither column carries a validity
/// buffer: the builder path appends no nulls, the spool path passes `None` explicitly, and a
/// spurious all-valid buffer would change the file's bytes.
fn write_keyed_array(
    path: &Path,
    terms: UInt32Array,
    postings: LargeBinaryArray,
) -> io::Result<()> {
    let schema = Arc::new(Schema::new(vec![
        Field::new(TERM_COLUMN_NAME, DataType::UInt32, false),
        Field::new(POSTING_COLUMN_NAME, DataType::LargeBinary, false),
    ]));
    let batch = RecordBatch::try_new(schema.clone(), vec![Arc::new(terms), Arc::new(postings)])
        .map_err(|e| invalid_data(e.to_string()))?;

    let file = File::create(path)?;
    let mut writer = FileWriter::try_new(std::io::BufWriter::new(file), &schema)
        .map_err(|e| invalid_data(e.to_string()))?;
    writer
        .write(&batch)
        .map_err(|e| invalid_data(e.to_string()))?;
    writer.finish().map_err(|e| invalid_data(e.to_string()))?;
    Ok(())
}

/// A keyed postings file written key by key, across as many bands as its producer needs.
///
/// **Streaming cannot be done by appending record batches.** [`DeltaTier::open`] decodes a single
/// batch and refuses a second, because its postings borrow from the mapping rather than copying —
/// concatenating batches is exactly the copy that construction exists to avoid. So this is the
/// spool-then-assemble discipline [`crate::PostingsSpool`] already applies to the positional
/// format: each encoded record is spooled as it arrives, and only the key and its Arrow offset are
/// held — twelve bytes per key, against the whole encoded posting set the buffered writer holds.
/// `finish` maps the spool as the posting column's values buffer and writes the one record batch
/// from it, byte for byte the file [`write_delta_tier_at`] would write from the same entries.
///
/// **The ascending-key check holds across bands, not merely within one.** It compares against the
/// last key appended, wherever that came from, which is what makes a banded producer safe: a band
/// boundary is not a place the ordering may lapse. The check is the same fail-closed rule
/// [`write_delta_tier_at`] states — the lookup is a binary search, so an unordered file answers
/// `None` for keys it holds and a duplicate makes one record unreachable, and a disappeared posting
/// is items a viewer is entitled to simply not being there.
pub struct KeyedPostingsSpool {
    spool_path: PathBuf,
    writer: std::io::BufWriter<File>,
    /// Arrow LargeBinary offsets: `offsets[i]..offsets[i + 1]` bounds record `i`; leading 0.
    offsets: Vec<i64>,
    keys: Vec<u32>,
    small_term_threshold: u32,
}

impl KeyedPostingsSpool {
    /// Create (truncating) the spool file at `spool_path`.
    pub fn create(spool_path: &Path, small_term_threshold: u32) -> io::Result<Self> {
        // Read access is required as well as write: `finish` memory-maps the spool through this
        // same handle, and mapping a write-only descriptor fails with EACCES.
        let file = std::fs::OpenOptions::new()
            .read(true)
            .write(true)
            .create(true)
            .truncate(true)
            .open(spool_path)?;
        Ok(KeyedPostingsSpool {
            spool_path: spool_path.to_path_buf(),
            writer: std::io::BufWriter::new(file),
            offsets: vec![0],
            keys: Vec::new(),
            small_term_threshold,
        })
    }

    /// Append one key's posting. `entities` must be sorted strictly ascending, which
    /// [`encode_posting`] enforces and says why; `key` must be strictly above every key appended
    /// before it, in this band or any earlier one.
    pub fn append(&mut self, key: u32, entities: &[u32]) -> io::Result<()> {
        if let Some(&last) = self.keys.last() {
            if key <= last {
                return Err(io::Error::new(
                    io::ErrorKind::InvalidInput,
                    format!(
                        "keyed postings: key {key} follows {last}, but keys must be strictly \
                         ascending (sorted, no duplicates)"
                    ),
                ));
            }
        }
        let record = encode_posting(key as usize, entities, self.small_term_threshold)?;
        let last = *self
            .offsets
            .last()
            .expect("offsets holds a leading 0 from create");
        let len = i64::try_from(record.len()).map_err(|_| {
            invalid_data(format!(
                "posting record of {} bytes is too large",
                record.len()
            ))
        })?;
        let next = last
            .checked_add(len)
            .ok_or_else(|| invalid_data("keyed postings spool exceeds i64::MAX total bytes"))?;
        self.writer.write_all(&record)?;
        self.offsets.push(next);
        self.keys.push(key);
        Ok(())
    }

    /// How many keys have been appended.
    pub fn len(&self) -> usize {
        self.keys.len()
    }

    pub fn is_empty(&self) -> bool {
        self.keys.is_empty()
    }

    /// Assemble the file at `path` from the spool, and delete the spool on success.
    pub fn finish(self, path: &Path) -> io::Result<()> {
        let file = self
            .writer
            .into_inner()
            .map_err(io::IntoInnerError::into_error)?;
        // The spool is about to be read back through a memory map; its bytes must be durable and
        // visible before the map is taken.
        file.sync_all()?;

        let total = *self
            .offsets
            .last()
            .expect("offsets holds a leading 0 from create");
        let total = usize::try_from(total).map_err(|_| {
            invalid_data("keyed postings spool total exceeds usize on this platform")
        })?;

        let values = if total == 0 {
            // memmap2 rejects zero-length maps; an empty values buffer is what the builder path
            // produces for zero records (and for all-empty records) anyway.
            Buffer::from_vec(Vec::<u8>::new())
        } else {
            let mapping = unsafe { memmap2::Mmap::map(&file) }?;
            if mapping.len() != total {
                return Err(invalid_data(format!(
                    "keyed postings spool is {} bytes but the offset table accounts for {total}",
                    mapping.len()
                )));
            }
            let arc: Arc<memmap2::Mmap> = Arc::new(mapping);
            // SAFETY: the same argument as `PostingsSpool::finish`'s — `arc` owns the mapping for
            // as long as any Buffer built from it is alive (captured as the buffer's
            // `Allocation`), the mapping is valid for `total` bytes for its entire lifetime, and
            // memmap2::Mmap never returns a null base pointer.
            let ptr = NonNull::new(arc.as_ptr() as *mut u8)
                .expect("memmap2::Mmap never returns a null base pointer");
            unsafe { Buffer::from_custom_allocation(ptr, total, arc) }
        };
        drop(file);

        let offsets =
            arrow::buffer::OffsetBuffer::new(arrow::buffer::ScalarBuffer::from(self.offsets));
        let postings = LargeBinaryArray::try_new(offsets, values, None)
            .map_err(|e| invalid_data(e.to_string()))?;
        write_keyed_array(path, UInt32Array::from(self.keys), postings)?;

        // The map over the spool was dropped with the array inside `write_keyed_array`; the spool
        // is only removed once the file is fully written.
        std::fs::remove_file(&self.spool_path)
    }
}

/// One live delta postings tier, memory-mapped. Postings borrow from the mapping without copying,
/// exactly as [`crate::PostingsReader`]'s do.
#[derive(Debug)]
pub struct DeltaTier {
    terms: UInt32Array,
    postings: LargeBinaryArray,
}

impl DeltaTier {
    pub fn open(path: &Path) -> io::Result<Self> {
        let file = File::open(path)?;
        let mapping = unsafe { memmap2::Mmap::map(&file) }?;
        let len = mapping.len();
        let arc: Arc<memmap2::Mmap> = Arc::new(mapping);
        // SAFETY: identical to `PostingsReader::open`'s — `arc` owns the mapping for as long as
        // any `Buffer` built from it is alive, the mapping is valid for `len` bytes for its whole
        // lifetime, and memmap2 never returns a null base pointer.
        let ptr = NonNull::new(arc.as_ptr() as *mut u8)
            .expect("memmap2::Mmap never returns a null base pointer");
        let buffer = unsafe { Buffer::from_custom_allocation(ptr, len, arc) };

        let batch = decode_single_batch(&buffer, "delta tier")?;
        if batch.num_columns() != 2 {
            return Err(invalid_data(format!(
                "delta tier: expected two columns, found {}",
                batch.num_columns()
            )));
        }
        let terms = batch
            .column(0)
            .as_any()
            .downcast_ref::<UInt32Array>()
            .ok_or_else(|| invalid_data("delta tier: column 0 is not a UInt32Array"))?
            .clone();
        let postings = batch
            .column(1)
            .as_any()
            .downcast_ref::<LargeBinaryArray>()
            .ok_or_else(|| invalid_data("delta tier: column 1 is not a LargeBinaryArray"))?
            .clone();
        if terms.len() != postings.len() {
            return Err(invalid_data(format!(
                "delta tier: {} term ids against {} postings",
                terms.len(),
                postings.len()
            )));
        }
        // The ordering the binary search below depends on, established once at open rather than
        // trusted per lookup — a file written by anything but `write_delta_tier` reaches here too.
        if !terms.values().windows(2).all(|w| w[0] < w[1]) {
            return Err(invalid_data(
                "delta tier: term ids are not strictly ascending, so a lookup could not find them",
            ));
        }
        crate::postings::validate_records(&postings)?;

        Ok(DeltaTier { terms, postings })
    }

    /// How many terms this tier carries — its record count, never a dictionary bound.
    pub fn term_count(&self) -> u32 {
        self.terms.len() as u32
    }

    /// The term ids this tier carries, ascending.
    ///
    /// Needed because [`Self::term_count`] is a **record count, not an id domain**: a tier holding
    /// one posting for term 5,000 has a count of 1, so walking `0..term_count()` finds nothing at
    /// all. Coalescing has to enumerate what is actually there.
    pub fn terms(&self) -> impl Iterator<Item = TermId> + '_ {
        self.terms.values().iter().map(|t| TermId::new(*t))
    }

    /// This tier's posting for `t`, or `None` if it carries none. `None` is an ordinary answer:
    /// a tier holds only the terms its flushed items carried.
    pub fn posting(&self, t: TermId) -> io::Result<Option<PostingRef<'_>>> {
        self.posting_at(t.raw())
    }

    /// The same lookup, addressed by a bare record ordinal — the format core
    /// ([`crate::PostingsReader::posting_at`] carries the argument for why both exist).
    pub fn posting_at(&self, ordinal: u32) -> io::Result<Option<PostingRef<'_>>> {
        let Ok(idx) = self.terms.values().binary_search(&ordinal) else {
            return Ok(None);
        };
        read_posting(&self.postings, idx).map(Some)
    }

    /// The record ordinals this tier carries, ascending — [`Self::terms`] untyped.
    pub fn ordinals(&self) -> impl Iterator<Item = u32> + '_ {
        self.terms.values().iter().copied()
    }

    /// [`Self::posting_at`]'s first half alone: the binary search over the key array.
    ///
    /// Exists so a bench can price the search and the view construction separately without
    /// transcribing either — `posting_at` is these two calls and nothing else, which is the
    /// property the decomposition depends on
    /// (`probes/2026-09-02-value-suggestion/`, the decomposition arm).
    #[cfg(feature = "bench-timing")]
    pub fn bench_record_index(&self, ordinal: u32) -> Option<usize> {
        self.terms.values().binary_search(&ordinal).ok()
    }

    /// [`Self::posting_at`]'s second half alone: the view over the mapped record's bytes.
    #[cfg(feature = "bench-timing")]
    pub fn bench_posting_at_index(&self, idx: usize) -> io::Result<PostingRef<'_>> {
        read_posting(&self.postings, idx)
    }
}

/// Coalesce several delta tiers into one, at `out`.
///
/// **A content-preserving re-encode, and that phrase is the specification.** The same
/// `(term, entity)` pairs the inputs carried, concatenated, deduplicated and re-sorted — nothing
/// dropped, nothing consulted. This is to postings exactly what the Morton merge-sort is to a
/// segment's codes.
///
/// **A merge retires nothing** (write-path §7). No tombstone is applied and no overlay entry
/// becomes retirable: a merge that dropped a posting because an entity was
/// deleted would be performing the compaction fold, which is invariant-bearing work this layer must
/// not do. The dedup is set semantics over identical pairs, so it changes no viewer's answer.
///
/// **The dedup is required, not defensive.** A buffered row's descriptors are not deduplicated on
/// the write path, and two tiers may legitimately carry the same `(term, entity)` — the same entity
/// appearing under one term in two flushes cannot happen, but the same term appearing in both tiers
/// certainly can, and concatenating their entity lists yields a non-ascending sequence.
/// [`encode_posting`] hard-fails on exactly that, so "concatenate and sort" without the dedup
/// specifies an artefact the encoder refuses to write.
///
/// Reads every input fully into memory: a tier holds one tick's arrivals, and the merge policy's
/// size bound is what keeps the total in hand.
pub fn coalesce_delta_tiers(
    inputs: &[PathBuf],
    out: &Path,
    small_term_threshold: u32,
) -> io::Result<()> {
    let mut by_term: BTreeMap<u32, Vec<u32>> = BTreeMap::new();
    for path in inputs {
        let tier = DeltaTier::open(path)?;
        for term in tier.terms().collect::<Vec<_>>() {
            let Some(posting) = tier.posting(term)? else {
                continue;
            };
            let entities = by_term.entry(term.raw()).or_default();
            match posting {
                PostingRef::Roaring(view) => entities.extend(view.iter()),
                PostingRef::Array(bytes) => {
                    for chunk in bytes.chunks_exact(4) {
                        entities.push(u32::from_le_bytes(chunk.try_into().unwrap()));
                    }
                }
            }
        }
    }

    let entries: Vec<(TermId, Vec<u32>)> = by_term
        .into_iter()
        .map(|(term, mut entities)| {
            entities.sort_unstable();
            entities.dedup();
            (TermId::new(term), entities)
        })
        .collect();
    write_delta_tier(out, &entries, small_term_threshold)
}
