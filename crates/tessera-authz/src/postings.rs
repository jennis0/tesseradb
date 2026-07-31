//! CSR postings writer and reader (Task 5, Reference Sheet R4).
//!
//! `terms/postings.arrow` is one Arrow IPC FILE holding a single record batch with one
//! `LargeBinaryArray` column named `posting`; row ordinal = term_id. Each record is
//! `u8 tag ‖ payload`: tag 0 is a sorted `u32` little-endian entity array (used when
//! `count <= small_term_threshold`, including the empty case); tag 1 is portable-serialised
//! Roaring bitmap bytes.
//!
//! This crate never depends on `tessera-store` or `tessera-spatial`, and `RowId` never
//! appears here — postings are entity-space only.

use std::fs::File;
use std::io::{self, BufWriter, Write};
use std::path::{Path, PathBuf};
use std::ptr::NonNull;
use std::sync::Arc;

use arrow::array::{Array, LargeBinaryArray, LargeBinaryBuilder};
use arrow::buffer::{Buffer, OffsetBuffer, ScalarBuffer};
use arrow::datatypes::{DataType, Field, Schema, SchemaRef};
use arrow::ipc::reader::{read_footer_length, FileDecoder};
use arrow::ipc::writer::FileWriter;
use arrow::ipc::{root_as_footer, MetadataVersion};
use arrow::record_batch::RecordBatch;
use croaring::{Bitmap, BitmapView, Portable};

use tessera_types::TermId;

const POSTING_COLUMN_NAME: &str = "posting";

/// Write `postings.arrow`: `per_term[t]` is term *t*'s sorted entity list. A record uses tag 0
/// (raw little-endian `u32` array) when `per_term[t].len() <= small_term_threshold as usize`
/// (including the empty case), and tag 1 (portable Roaring bytes) otherwise.
///
/// `per_term[t]` must already be sorted strictly ascending (no duplicates) — this is a CSR
/// postings writer, not a sort step; callers are expected to hand it term-ordered, sorted entity
/// lists (e.g. from the build pipeline's grouping pass). This is checked unconditionally
/// (not just in debug builds): these are authorisation masks, and an unsorted/duplicated input
/// would otherwise silently diverge in content depending only on which side of
/// `small_term_threshold` a term's count lands (tag 0 stores input verbatim; tag 1 sorts and
/// dedups via the Roaring bitmap) — a content-integrity bug, not merely a style one.
pub fn write_postings(
    path: &Path,
    per_term: &[Vec<u32>],
    small_term_threshold: u32,
) -> io::Result<()> {
    let mut records = Vec::with_capacity(per_term.len());
    for (t, entities) in per_term.iter().enumerate() {
        records.push(encode_posting(t, entities, small_term_threshold)?);
    }
    write_posting_records(path, &records)
}

/// Encode term `t`'s sorted entity list into its on-disk record (`u8 tag ‖ payload`), applying
/// exactly the tag rule [`write_postings`] documents and the same unconditional sortedness
/// check. Split out so a build that cannot hold every term's entity list at once can encode
/// each term as soon as its list is complete and keep only the (compressed) records.
pub fn encode_posting(
    t: usize,
    entities: &[u32],
    small_term_threshold: u32,
) -> io::Result<Vec<u8>> {
    if !entities.windows(2).all(|w| w[0] < w[1]) {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            format!(
                "write_postings: term {t}'s entity list must be sorted strictly ascending \
                 (no duplicates)"
            ),
        ));
    }

    let mut record = Vec::new();
    if (entities.len() as u64) <= small_term_threshold as u64 {
        record.push(0u8);
        for entity in entities {
            record.extend_from_slice(&entity.to_le_bytes());
        }
    } else {
        let mut bitmap = Bitmap::of(entities);
        bitmap.run_optimize();
        record.push(1u8);
        record.extend_from_slice(&bitmap.serialize::<Portable>());
    }
    Ok(record)
}

/// Write `postings.arrow` from already-encoded records (see [`encode_posting`]); record ordinal
/// = term id. Byte-for-byte the same file [`write_postings`] would write from the same postings.
pub fn write_posting_records(path: &Path, records: &[Vec<u8>]) -> io::Result<()> {
    let mut builder = LargeBinaryBuilder::new();
    for record in records {
        builder.append_value(record);
    }
    write_posting_array(path, builder.finish())
}

/// The single schema/batch/IPC-writer invocation behind [`write_posting_records`] and
/// [`PostingsSpool::finish`]. Byte-identity between the buffered and spooled paths requires
/// this to be literally the same code, not two copies that could drift. The column carries no
/// validity buffer: the builder path appends no nulls (so its null buffer is `None`) and the
/// spool path passes `None` explicitly — a spurious all-valid buffer would change the file
/// bytes.
fn write_posting_array(path: &Path, array: LargeBinaryArray) -> io::Result<()> {
    let schema = Arc::new(Schema::new(vec![Field::new(
        POSTING_COLUMN_NAME,
        DataType::LargeBinary,
        false,
    )]));
    let batch = RecordBatch::try_new(schema.clone(), vec![Arc::new(array)])
        .map_err(|e| io::Error::new(io::ErrorKind::InvalidData, e.to_string()))?;

    let file = File::create(path)?;
    let mut writer = FileWriter::try_new(BufWriter::new(file), &schema)
        .map_err(|e| io::Error::new(io::ErrorKind::InvalidData, e.to_string()))?;
    writer
        .write(&batch)
        .map_err(|e| io::Error::new(io::ErrorKind::InvalidData, e.to_string()))?;
    writer
        .finish()
        .map_err(|e| io::Error::new(io::ErrorKind::InvalidData, e.to_string()))?;
    Ok(())
}

/// Streaming counterpart to [`write_posting_records`]: encoded records (see [`encode_posting`])
/// are spooled to a temporary file as they arrive — record ordinal = term id — with only the
/// Arrow offset table held in memory (one `i64` per record, plus a leading zero). `finish`
/// memory-maps the spool as the column's values buffer and writes `postings.arrow` through the
/// same IPC-writer invocation as [`write_posting_records`], so the output is byte-for-byte the
/// file that function would write from the same records; the reader's single-record-batch
/// layout constraint (see [`PostingsReader`]) is met the same way, with one batch.
pub struct PostingsSpool {
    spool_path: PathBuf,
    writer: BufWriter<File>,
    // Arrow LargeBinary offsets: offsets[t]..offsets[t + 1] bounds record t; leading 0.
    offsets: Vec<i64>,
}

impl PostingsSpool {
    /// Create (truncating) the spool file at `spool_path`.
    pub fn create(spool_path: &Path) -> io::Result<Self> {
        // Read access is required as well as write: `finish` memory-maps the spool through this
        // same handle, and mapping a write-only descriptor fails with EACCES.
        let file = std::fs::OpenOptions::new()
            .read(true)
            .write(true)
            .create(true)
            .truncate(true)
            .open(spool_path)?;
        Ok(PostingsSpool {
            spool_path: spool_path.to_path_buf(),
            writer: BufWriter::new(file),
            offsets: vec![0],
        })
    }

    /// Append the next term's encoded record. Records must arrive in term order; ordinal in the
    /// finished file = term id.
    pub fn append(&mut self, record: &[u8]) -> io::Result<()> {
        let last = *self
            .offsets
            .last()
            .expect("offsets holds a leading 0 from create");
        let next = next_offset(last, record.len())?;
        self.writer.write_all(record)?;
        self.offsets.push(next);
        Ok(())
    }

    /// Flush and fsync the spool, write `postings.arrow` at `postings_path` from it, and delete
    /// the spool file on success.
    pub fn finish(self, postings_path: &Path) -> io::Result<()> {
        let file = self
            .writer
            .into_inner()
            .map_err(io::IntoInnerError::into_error)?;
        // The spool is about to be read back through a memory map; its bytes must be durable
        // and visible before the map is taken.
        file.sync_all()?;

        let total = *self
            .offsets
            .last()
            .expect("offsets holds a leading 0 from create");
        let total = usize::try_from(total).map_err(|_| {
            invalid_data("postings spool total exceeds usize on this platform")
        })?;

        let values = if total == 0 {
            // memmap2 rejects zero-length maps; an empty values buffer is what the builder
            // path produces for zero records (and for all-empty records) anyway.
            Buffer::from_vec(Vec::<u8>::new())
        } else {
            let mapping = unsafe { memmap2::Mmap::map(&file) }?;
            if mapping.len() != total {
                return Err(invalid_data(format!(
                    "postings spool is {} bytes but the offset table accounts for {total}",
                    mapping.len()
                )));
            }
            let arc: Arc<memmap2::Mmap> = Arc::new(mapping);
            // SAFETY: same argument as `PostingsReader::open`'s mmap arm — `arc` owns the
            // mapping for as long as any Buffer built from it is alive (captured as the
            // buffer's `Allocation`), the mapping is valid for `total` bytes for its entire
            // lifetime, and memmap2::Mmap never returns a null base pointer.
            let ptr = NonNull::new(arc.as_ptr() as *mut u8)
                .expect("memmap2::Mmap never returns a null base pointer");
            unsafe { Buffer::from_custom_allocation(ptr, total, arc) }
        };
        drop(file);

        let offsets = OffsetBuffer::new(ScalarBuffer::from(self.offsets));
        let array = LargeBinaryArray::try_new(offsets, values, None)
            .map_err(|e| invalid_data(e.to_string()))?;
        write_posting_array(postings_path, array)?;

        // The map over the spool was dropped with the array inside `write_posting_array`;
        // the spool is only removed once `postings.arrow` is fully written.
        std::fs::remove_file(&self.spool_path)
    }
}

/// Bounds-check the next Arrow offset. LargeBinary offsets are `i64`; a spool whose running
/// total would exceed `i64::MAX` cannot be represented and must fail closed, not wrap.
fn next_offset(last: i64, record_len: usize) -> io::Result<i64> {
    let len = i64::try_from(record_len).map_err(|_| {
        io::Error::new(
            io::ErrorKind::InvalidInput,
            format!("posting record of {record_len} bytes exceeds i64::MAX"),
        )
    })?;
    last.checked_add(len).ok_or_else(|| {
        io::Error::new(
            io::ErrorKind::InvalidInput,
            "postings spool exceeds i64::MAX total bytes",
        )
    })
}

/// A borrowed view of one term's postings, tied to the lifetime of the [`PostingsReader`] that
/// produced it.
#[derive(Debug)]
pub enum PostingRef<'a> {
    /// Tag 0: raw little-endian `u32` entity ids, sorted ascending.
    Array(&'a [u8]),
    /// Tag 1: a Roaring bitmap view deserialised (no-copy, no-alignment-required) from the
    /// portable wire format.
    Roaring(BitmapView<'a>),
}

/// Reads `postings.arrow`. Holds the backing bytes (either an owned buffer or a memory map);
/// [`PostingRef`]s returned by [`PostingsReader::posting`] borrow from that backing storage
/// without copying.
#[derive(Debug)]
pub struct PostingsReader {
    array: LargeBinaryArray,
}

impl PostingsReader {
    /// Open `path`. When `mmap` is `true`, the file is memory-mapped and the record batch is
    /// decoded zero-copy from the map; when `false`, the file is read into an owned buffer and
    /// decoded zero-copy from that buffer instead. Either way, no per-record copy happens on
    /// open or on lookup.
    pub fn open(path: &Path, mmap: bool) -> io::Result<Self> {
        let buffer = if mmap {
            let file = File::open(path)?;
            let mapping = unsafe { memmap2::Mmap::map(&file) }?;
            let len = mapping.len();
            let arc: Arc<memmap2::Mmap> = Arc::new(mapping);
            // SAFETY: `arc` owns the mapping for as long as any Buffer built from it is alive
            // (the Arc is captured as the buffer's `Allocation`), and the mapping is valid for
            // `len` bytes for its entire lifetime.
            // memmap2::Mmap never returns a null base pointer (even the zero-length map case
            // uses a valid, non-null dangling-style allocation internally) — see memmap2's
            // `MmapInner` construction, which always goes through a real `mmap(2)`/`VirtualAlloc`
            // call or a dedicated empty-map sentinel address, never a null pointer.
            let ptr = NonNull::new(arc.as_ptr() as *mut u8)
                .expect("memmap2::Mmap never returns a null base pointer");
            unsafe { Buffer::from_custom_allocation(ptr, len, arc) }
        } else {
            let data = std::fs::read(path)?;
            Buffer::from_vec(data)
        };

        let batch = decode_single_batch(&buffer)?;

        if batch.num_columns() != 1 {
            return Err(invalid_data(format!(
                "postings.arrow: expected exactly one column, found {}",
                batch.num_columns()
            )));
        }
        let field = batch.schema_ref().field(0).clone();
        if field.name() != POSTING_COLUMN_NAME || field.data_type() != &DataType::LargeBinary {
            return Err(invalid_data(format!(
                "postings.arrow: expected LargeBinary column named '{POSTING_COLUMN_NAME}', \
                 found '{}' ({:?})",
                field.name(),
                field.data_type()
            )));
        }

        let array = batch
            .column(0)
            .as_any()
            .downcast_ref::<LargeBinaryArray>()
            .ok_or_else(|| invalid_data("postings.arrow: column 0 is not a LargeBinaryArray"))?
            .clone();

        validate_records(&array)?;

        Ok(PostingsReader { array })
    }

    /// The number of terms (records) in this postings file.
    pub fn term_count(&self) -> u32 {
        self.array.len() as u32
    }

    /// Return term `t`'s postings.
    pub fn posting(&self, t: TermId) -> io::Result<PostingRef<'_>> {
        let idx = t.raw() as usize;
        if idx >= self.array.len() {
            return Err(invalid_data(format!(
                "postings.arrow: term id {} out of range (term_count = {})",
                t.raw(),
                self.array.len()
            )));
        }

        let bytes = self.array.value(idx);
        let (tag, payload) = bytes
            .split_first()
            .ok_or_else(|| invalid_data(format!("postings.arrow: term {idx} has no tag byte")))?;

        match tag {
            0 => Ok(PostingRef::Array(payload)),
            1 => {
                // SAFETY: every tag-1 payload in `self.array` was validated once, at `open`
                // time, by `validate_records` — which round-trips it through
                // `Bitmap::try_deserialize::<Portable>` (bounds-checked, internally validated)
                // and confirms the payload is *exactly* the bitmap's serialised size with no
                // truncation or trailing garbage. `BitmapView::deserialize`'s own safety
                // contract (valid portable bytes, no length mismatch) is therefore already
                // discharged before we ever reach this unsafe block.
                let view = unsafe { BitmapView::deserialize::<Portable>(payload) };
                Ok(PostingRef::Roaring(view))
            }
            other => Err(invalid_data(format!(
                "postings.arrow: term {idx} has unknown tag byte {other}"
            ))),
        }
    }
}

/// Validate every record in `array` once, at `open` time, so that later lookups (which use the
/// unsafe zero-copy `BitmapView::deserialize` for tag-1 records) never operate on unchecked
/// bytes. A malformed record here — corrupt file, truncated write, wrong tag — fails `open`
/// closed (`InvalidData`) rather than causing undefined behaviour or a panic deep inside
/// CRoaring on first lookup.
fn validate_records(array: &LargeBinaryArray) -> io::Result<()> {
    for idx in 0..array.len() {
        let bytes = array.value(idx);
        let (tag, payload) = bytes
            .split_first()
            .ok_or_else(|| invalid_data(format!("postings.arrow: term {idx} has no tag byte")))?;

        match tag {
            0 => {
                if payload.len() % 4 != 0 {
                    return Err(invalid_data(format!(
                        "postings.arrow: term {idx} tag-0 payload length {} is not a multiple \
                         of 4",
                        payload.len()
                    )));
                }
            }
            1 => {
                let bitmap = Bitmap::try_deserialize::<Portable>(payload).ok_or_else(|| {
                    invalid_data(format!(
                        "postings.arrow: term {idx} tag-1 payload is not a valid portable \
                         Roaring bitmap"
                    ))
                })?;
                let consumed = bitmap.get_serialized_size_in_bytes::<Portable>();
                if consumed != payload.len() {
                    return Err(invalid_data(format!(
                        "postings.arrow: term {idx} tag-1 payload has {} trailing byte(s) \
                         beyond the {consumed}-byte serialised bitmap (payload is \
                         {} bytes) — BitmapView::deserialize requires an exact-length buffer",
                        payload.len().saturating_sub(consumed),
                        payload.len()
                    )));
                }
            }
            other => {
                return Err(invalid_data(format!(
                    "postings.arrow: term {idx} has unknown tag byte {other}"
                )))
            }
        }
    }
    Ok(())
}

/// Decode the (single) record batch of an Arrow IPC FILE held in `buffer`, without copying its
/// buffers (subject to alignment — see [`FileDecoder::with_require_alignment`]'s default).
fn decode_single_batch(buffer: &Buffer) -> io::Result<RecordBatch> {
    const FOOTER_TRAILER_LEN: usize = 10; // 4-byte footer length + 6-byte "ARROW1" magic
    if buffer.len() < FOOTER_TRAILER_LEN {
        return Err(invalid_data(
            "postings.arrow: file too short to contain a footer",
        ));
    }

    let trailer_start = buffer.len() - FOOTER_TRAILER_LEN;
    let trailer: [u8; FOOTER_TRAILER_LEN] = buffer[trailer_start..]
        .try_into()
        .expect("slice length matches FOOTER_TRAILER_LEN");
    let footer_len =
        read_footer_length(trailer).map_err(|e| invalid_data(format!("postings.arrow: {e}")))?;
    if footer_len > trailer_start {
        return Err(invalid_data(
            "postings.arrow: footer length exceeds file size",
        ));
    }

    let footer = root_as_footer(&buffer[trailer_start - footer_len..trailer_start])
        .map_err(|e| invalid_data(format!("postings.arrow: invalid footer: {e}")))?;

    let schema_fb = footer
        .schema()
        .ok_or_else(|| invalid_data("postings.arrow: footer has no schema"))?;
    let schema: SchemaRef = Arc::new(arrow::ipc::convert::fb_to_schema(schema_fb));

    let version: MetadataVersion = footer.version();
    let mut decoder = FileDecoder::new(schema, version);

    if let Some(dictionaries) = footer.dictionaries() {
        for block in dictionaries.iter() {
            let (offset, block_len) = checked_block_range(block, buffer.len())?;
            let data = buffer.slice_with_length(offset, block_len);
            decoder
                .read_dictionary(block, &data)
                .map_err(|e| invalid_data(format!("postings.arrow: {e}")))?;
        }
    }

    let batches = footer
        .recordBatches()
        .ok_or_else(|| invalid_data("postings.arrow: footer has no record batches"))?;
    if batches.len() != 1 {
        return Err(invalid_data(format!(
            "postings.arrow: expected exactly one record batch, found {}",
            batches.len()
        )));
    }

    let block = batches.get(0);
    let (offset, block_len) = checked_block_range(block, buffer.len())?;
    let data = buffer.slice_with_length(offset, block_len);

    decoder
        .read_record_batch(block, &data)
        .map_err(|e| invalid_data(format!("postings.arrow: {e}")))?
        .ok_or_else(|| invalid_data("postings.arrow: record batch block decoded to nothing"))
}

/// Validate a footer `Block`'s `(offset, bodyLength + metaDataLength)` against the file length,
/// returning them as checked `usize`s. `Block`'s fields are `i64` in the flatbuffer schema; a
/// corrupt or adversarial footer could report a negative value, an overflowing sum, or a range
/// past end-of-file — `Buffer::slice_with_length` panics on out-of-bounds input, so every field
/// must be checked here before it ever reaches that call (fail closed, not a panic).
fn checked_block_range(block: &arrow::ipc::Block, buffer_len: usize) -> io::Result<(usize, usize)> {
    let offset = usize::try_from(block.offset())
        .map_err(|_| invalid_data("postings.arrow: block offset is negative"))?;
    let body_len = usize::try_from(block.bodyLength())
        .map_err(|_| invalid_data("postings.arrow: block bodyLength is negative"))?;
    let meta_len = usize::try_from(block.metaDataLength())
        .map_err(|_| invalid_data("postings.arrow: block metaDataLength is negative"))?;
    let block_len = body_len
        .checked_add(meta_len)
        .ok_or_else(|| invalid_data("postings.arrow: block length overflows"))?;
    let end = offset
        .checked_add(block_len)
        .ok_or_else(|| invalid_data("postings.arrow: block offset + length overflows"))?;
    if end > buffer_len {
        return Err(invalid_data(format!(
            "postings.arrow: block range [{offset}, {end}) exceeds file length {buffer_len}"
        )));
    }
    Ok((offset, block_len))
}

fn invalid_data(msg: impl Into<String>) -> io::Error {
    io::Error::new(io::ErrorKind::InvalidData, msg.into())
}

#[cfg(test)]
mod tests {
    use super::*;
    use proptest::prelude::*;
    use rand::SeedableRng;
    use std::collections::BTreeSet;
    use tempfile::TempDir;

    #[test]
    fn threshold_boundary_32_is_array_33_is_roaring() {
        let temp = TempDir::new().unwrap();
        let path = temp.path().join("postings.arrow");

        let at_threshold: Vec<u32> = (0..32).collect();
        let over_threshold: Vec<u32> = (0..33).collect();
        write_postings(&path, &[at_threshold.clone(), over_threshold.clone()], 32).unwrap();

        let reader = PostingsReader::open(&path, false).unwrap();
        match reader.posting(TermId::new(0)).unwrap() {
            PostingRef::Array(bytes) => assert_eq!(bytes.len(), 32 * 4),
            PostingRef::Roaring(_) => panic!("count == threshold must stay tag 0"),
        };
        match reader.posting(TermId::new(1)).unwrap() {
            PostingRef::Roaring(bm) => assert_eq!(bm.cardinality(), 33),
            PostingRef::Array(_) => panic!("count == threshold + 1 must be tag 1"),
        };
    }

    proptest! {
        #[test]
        fn random_per_term_sets_round_trip(
            seed in any::<u64>(),
            term_sizes in prop::collection::vec(0usize..200, 1..12),
            mmap in any::<bool>(),
        ) {
            let mut rng = rand::rngs::StdRng::seed_from_u64(seed);
            let mut per_term = Vec::new();
            for size in term_sizes {
                let mut set: BTreeSet<u32> = BTreeSet::new();
                while set.len() < size {
                    set.insert(rand::Rng::gen_range(&mut rng, 0..1_000_000u32));
                }
                // BTreeSet iterates in ascending order already — no shuffle-then-sort needed.
                per_term.push(set.into_iter().collect::<Vec<u32>>());
            }

            let temp = TempDir::new().unwrap();
            let path = temp.path().join("postings.arrow");
            write_postings(&path, &per_term, 32).unwrap();

            let reader = PostingsReader::open(&path, mmap).unwrap();
            prop_assert_eq!(reader.term_count() as usize, per_term.len());

            for (t, expected) in per_term.iter().enumerate() {
                let got: Vec<u32> = match reader.posting(TermId::new(t as u32)).unwrap() {
                    PostingRef::Array(bytes) => bytes
                        .chunks_exact(4)
                        .map(|c| u32::from_le_bytes(c.try_into().unwrap()))
                        .collect(),
                    PostingRef::Roaring(bm) => bm.iter().collect(),
                };
                prop_assert_eq!(&got, expected);
            }
        }
    }

    /// The i64 offset overflow cannot be reached with real writes (it needs > 8 EiB of spool),
    /// so the guard is exercised directly.
    #[test]
    fn next_offset_rejects_i64_overflow() {
        assert_eq!(next_offset(i64::MAX - 4, 4).unwrap(), i64::MAX);
        let err = next_offset(i64::MAX, 1).unwrap_err();
        assert_eq!(err.kind(), io::ErrorKind::InvalidInput);
        let err = next_offset(i64::MAX - 3, 4).unwrap_err();
        assert_eq!(err.kind(), io::ErrorKind::InvalidInput);
    }

    #[test]
    fn write_postings_rejects_unsorted_input() {
        let temp = TempDir::new().unwrap();
        let path = temp.path().join("postings.arrow");
        let err = write_postings(&path, &[vec![5, 3, 9]], 32).unwrap_err();
        assert_eq!(err.kind(), io::ErrorKind::InvalidInput);
    }

    #[test]
    fn write_postings_rejects_duplicate_entities() {
        let temp = TempDir::new().unwrap();
        let path = temp.path().join("postings.arrow");
        let err = write_postings(&path, &[vec![3, 3, 9]], 32).unwrap_err();
        assert_eq!(err.kind(), io::ErrorKind::InvalidInput);
    }

    /// Cross-check the on-disk bytes for the singleton (tag 0) and large (tag 1) records against
    /// an independent Arrow reader (not `PostingsReader`) — proves the writer's byte layout,
    /// not just that our own reader agrees with itself.
    #[test]
    fn record_bytes_match_the_tagged_format_exactly() {
        let temp = TempDir::new().unwrap();
        let path = temp.path().join("postings.arrow");

        let singleton: Vec<u32> = vec![5];
        let large: Vec<u32> = (1..=1000u32).collect();
        write_postings(&path, &[singleton, large], 32).unwrap();

        let file = File::open(&path).unwrap();
        let mut reader = arrow::ipc::reader::FileReader::try_new(file, None).unwrap();
        let batch = reader.next().unwrap().unwrap();
        assert!(reader.next().is_none(), "expected exactly one record batch");

        let array = batch
            .column(0)
            .as_any()
            .downcast_ref::<LargeBinaryArray>()
            .unwrap();

        // Term 0: tag 0 ‖ u32 LE 5 -> exactly [0, 5, 0, 0, 0].
        assert_eq!(array.value(0), &[0u8, 5, 0, 0, 0]);

        // Term 1: tag 1, first payload byte is a portable-format control byte, not asserted
        // further here (that's what the round-trip test is for) — just the tag.
        assert_eq!(array.value(1)[0], 1u8);
    }

    #[test]
    fn open_rejects_wrong_column_schema() {
        let temp = TempDir::new().unwrap();
        let path = temp.path().join("postings.arrow");
        write_wrong_type_column(&path).unwrap();

        let err = PostingsReader::open(&path, false).unwrap_err();
        assert_eq!(err.kind(), io::ErrorKind::InvalidData);
    }

    #[test]
    fn open_rejects_unknown_tag_byte() {
        let temp = TempDir::new().unwrap();
        let path = temp.path().join("postings.arrow");
        write_raw_records(&path, &[&[2u8, 1, 2, 3, 4]]).unwrap();

        let err = PostingsReader::open(&path, false).unwrap_err();
        assert_eq!(err.kind(), io::ErrorKind::InvalidData);
    }

    #[test]
    fn open_rejects_zero_length_record() {
        let temp = TempDir::new().unwrap();
        let path = temp.path().join("postings.arrow");
        write_raw_records(&path, &[&[]]).unwrap();

        let err = PostingsReader::open(&path, false).unwrap_err();
        assert_eq!(err.kind(), io::ErrorKind::InvalidData);
    }

    #[test]
    fn posting_rejects_out_of_range_term_id() {
        let temp = TempDir::new().unwrap();
        let path = temp.path().join("postings.arrow");
        write_postings(&path, &[vec![1, 2, 3]], 32).unwrap();

        let reader = PostingsReader::open(&path, false).unwrap();
        let err = reader.posting(TermId::new(5)).unwrap_err();
        assert_eq!(err.kind(), io::ErrorKind::InvalidData);
    }

    #[test]
    fn open_rejects_truncated_tag1_record() {
        let temp = TempDir::new().unwrap();
        let valid_path = temp.path().join("valid.arrow");
        let large: Vec<u32> = (1..=1000u32).collect();
        write_postings(&valid_path, &[large], 32).unwrap();

        // Pull out a genuine tag-1 payload, then truncate it before re-embedding it as a
        // hand-built record — this must fail `open`, not walk off the end of the slice inside
        // CRoaring.
        let file = File::open(&valid_path).unwrap();
        let mut reader = arrow::ipc::reader::FileReader::try_new(file, None).unwrap();
        let batch = reader.next().unwrap().unwrap();
        let array = batch
            .column(0)
            .as_any()
            .downcast_ref::<LargeBinaryArray>()
            .unwrap();
        let full_record = array.value(0);
        assert_eq!(full_record[0], 1u8, "expected a tag-1 record to truncate");
        let truncated = &full_record[..full_record.len() / 2];

        let truncated_path = temp.path().join("truncated.arrow");
        write_raw_records(&truncated_path, &[truncated]).unwrap();

        let err = PostingsReader::open(&truncated_path, false).unwrap_err();
        assert_eq!(err.kind(), io::ErrorKind::InvalidData);
    }

    /// Build a `postings.arrow`-shaped file with arbitrary raw record bytes (bypassing
    /// `write_postings`'s tag encoding entirely), so tests can construct malformed records.
    fn write_raw_records(path: &Path, records: &[&[u8]]) -> io::Result<()> {
        let mut builder = LargeBinaryBuilder::new();
        for record in records {
            builder.append_value(record);
        }
        let array = builder.finish();
        let schema = Arc::new(Schema::new(vec![Field::new(
            POSTING_COLUMN_NAME,
            DataType::LargeBinary,
            false,
        )]));
        let batch = RecordBatch::try_new(schema.clone(), vec![Arc::new(array)])
            .map_err(|e| io::Error::new(io::ErrorKind::InvalidData, e.to_string()))?;

        let file = File::create(path)?;
        let mut writer = FileWriter::try_new(BufWriter::new(file), &schema)
            .map_err(|e| io::Error::new(io::ErrorKind::InvalidData, e.to_string()))?;
        writer
            .write(&batch)
            .map_err(|e| io::Error::new(io::ErrorKind::InvalidData, e.to_string()))?;
        writer
            .finish()
            .map_err(|e| io::Error::new(io::ErrorKind::InvalidData, e.to_string()))?;
        Ok(())
    }

    /// Build a `postings.arrow`-shaped file whose single column is named `posting` but has the
    /// wrong Arrow type (`UInt32` instead of `LargeBinary`) — exercises the schema check in
    /// `open`.
    fn write_wrong_type_column(path: &Path) -> io::Result<()> {
        use arrow::array::UInt32Array;

        let array = UInt32Array::from(vec![1u32, 2, 3]);
        let schema = Arc::new(Schema::new(vec![Field::new(
            POSTING_COLUMN_NAME,
            DataType::UInt32,
            false,
        )]));
        let batch = RecordBatch::try_new(schema.clone(), vec![Arc::new(array)])
            .map_err(|e| io::Error::new(io::ErrorKind::InvalidData, e.to_string()))?;

        let file = File::create(path)?;
        let mut writer = FileWriter::try_new(BufWriter::new(file), &schema)
            .map_err(|e| io::Error::new(io::ErrorKind::InvalidData, e.to_string()))?;
        writer
            .write(&batch)
            .map_err(|e| io::Error::new(io::ErrorKind::InvalidData, e.to_string()))?;
        writer
            .finish()
            .map_err(|e| io::Error::new(io::ErrorKind::InvalidData, e.to_string()))?;
        Ok(())
    }
}
