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
use std::io::{self, BufWriter};
use std::path::Path;
use std::ptr::NonNull;
use std::sync::Arc;

use arrow::array::{Array, LargeBinaryArray, LargeBinaryBuilder};
use arrow::buffer::Buffer;
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
/// `per_term[t]` must already be sorted ascending — this is a CSR postings writer, not a sort
/// step; callers are expected to hand it term-ordered, sorted entity lists (e.g. from the build
/// pipeline's grouping pass).
pub fn write_postings(
    path: &Path,
    per_term: &[Vec<u32>],
    small_term_threshold: u32,
) -> io::Result<()> {
    let mut builder = LargeBinaryBuilder::new();

    for entities in per_term {
        debug_assert!(
            entities.windows(2).all(|w| w[0] <= w[1]),
            "write_postings: entity ids for a term must be sorted ascending"
        );

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
        builder.append_value(&record);
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

/// A borrowed view of one term's postings, tied to the lifetime of the [`PostingsReader`] that
/// produced it.
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
            let ptr = NonNull::new(arc.as_ptr() as *mut u8).unwrap_or_else(NonNull::dangling);
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
                // SAFETY: `payload` is exactly the bytes produced by `write_postings` via
                // `Bitmap::serialize::<Portable>`, so it is a valid portable Roaring bitmap.
                let view = unsafe { BitmapView::deserialize::<Portable>(payload) };
                Ok(PostingRef::Roaring(view))
            }
            other => Err(invalid_data(format!(
                "postings.arrow: term {idx} has unknown tag byte {other}"
            ))),
        }
    }
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
            let block_len = (block.bodyLength() + block.metaDataLength() as i64) as usize;
            let offset = block.offset() as usize;
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
    let block_len = (block.bodyLength() + block.metaDataLength() as i64) as usize;
    let offset = block.offset() as usize;
    let data = buffer.slice_with_length(offset, block_len);

    decoder
        .read_record_batch(block, &data)
        .map_err(|e| invalid_data(format!("postings.arrow: {e}")))?
        .ok_or_else(|| invalid_data("postings.arrow: record batch block decoded to nothing"))
}

fn invalid_data(msg: impl Into<String>) -> io::Error {
    io::Error::new(io::ErrorKind::InvalidData, msg.into())
}

#[cfg(test)]
mod tests {
    use super::*;
    use proptest::prelude::*;
    use rand::seq::SliceRandom;
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
        ) {
            let mut rng = rand::rngs::StdRng::seed_from_u64(seed);
            let mut per_term = Vec::new();
            for size in term_sizes {
                let mut set: BTreeSet<u32> = BTreeSet::new();
                while set.len() < size {
                    set.insert(rand::Rng::gen_range(&mut rng, 0..1_000_000u32));
                }
                let mut v: Vec<u32> = set.into_iter().collect();
                v.shuffle(&mut rng);
                v.sort_unstable();
                per_term.push(v);
            }

            let temp = TempDir::new().unwrap();
            let path = temp.path().join("postings.arrow");
            write_postings(&path, &per_term, 32).unwrap();

            let reader = PostingsReader::open(&path, false).unwrap();
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
}
