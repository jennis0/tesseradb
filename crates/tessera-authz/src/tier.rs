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

use std::fs::File;
use std::io;
use std::path::Path;
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
    if !entries.windows(2).all(|w| w[0].0.raw() < w[1].0.raw()) {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "write_delta_tier: term ids must be strictly ascending (sorted, no duplicates)",
        ));
    }

    let mut terms: Vec<u32> = Vec::with_capacity(entries.len());
    let mut postings = LargeBinaryBuilder::new();
    for (term, entities) in entries {
        terms.push(term.raw());
        postings.append_value(encode_posting(
            term.raw() as usize,
            entities,
            small_term_threshold,
        )?);
    }

    let schema = Arc::new(Schema::new(vec![
        Field::new(TERM_COLUMN_NAME, DataType::UInt32, false),
        Field::new(POSTING_COLUMN_NAME, DataType::LargeBinary, false),
    ]));
    let batch = RecordBatch::try_new(
        schema.clone(),
        vec![
            Arc::new(UInt32Array::from(terms)),
            Arc::new(postings.finish()),
        ],
    )
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

        let batch = decode_single_batch(&buffer)?;
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

    /// This tier's posting for `t`, or `None` if it carries none. `None` is an ordinary answer:
    /// a tier holds only the terms its flushed items carried.
    pub fn posting(&self, t: TermId) -> io::Result<Option<PostingRef<'_>>> {
        let Ok(idx) = self.terms.values().binary_search(&t.raw()) else {
            return Ok(None);
        };
        read_posting(&self.postings, idx).map(Some)
    }
}
