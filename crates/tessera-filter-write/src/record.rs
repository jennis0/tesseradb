//! Writing the record blob: rows in, three files out (`records-and-search.md` §3, §7).
//!
//! The byte format — rows, blocks, the directory — is owned by `tessera_filter::record`, and this
//! writer is its producer half: it lives in this crate for the module doc's codegen reason (the
//! scan's crate holds no write machinery it can avoid holding), and it calls
//! [`tessera_filter::encode_row`] rather than encoding anything itself, so the layout stays one
//! module's fact.
//!
//! Rows arrive in **strictly ascending entity order** — the order the has-row rank addresses them
//! back in — and are cut into blocks against the caller's uncompressed target: a block seals when
//! the next row would pass it, so a row larger than the target gets an oversized block of its own
//! (records §3: the target is a target, not a cap). Non-ascending entities are refused rather
//! than sorted, for `merge_order`'s reason: a sort here would paper over a broken allocator, and
//! the symptom would be rows addressed against the wrong ranks.
//!
//! Block bytes stream to `blocks.bin` as blocks seal, so the writer holds one uncompressed block
//! plus the directory's bookkeeping (a handful of words per block, 4 B per row) — never the blob.
//! A writer abandoned part-way leaves a partial `blocks.bin` behind; no manifest names it, and
//! the next build truncates it at create.

use std::fs::File;
use std::io::{self, BufWriter, Write};
use std::path::{Path, PathBuf};
use std::sync::Arc;

use arrow::array::{ArrayRef, LargeListArray, RecordBatch, UInt32Array, UInt64Array};
use arrow::buffer::{OffsetBuffer, ScalarBuffer};
use arrow::datatypes::{DataType, Field, Schema};
use croaring::{Bitmap, Portable};

use tessera_filter::{encode_row, RecordField};

/// zstd's default level — the operating point the string-storage probe measured its block ratios
/// at. A writer's choice, not a format fact: the reader decompresses whatever level wrote the
/// frame.
const ZSTD_LEVEL: i32 = 3;

/// Streams one record blob: `blocks.bin` as rows arrive, `hasrow.roaring` and `directory.arrow`
/// at [`RecordBlobWriter::finish`]. Every producer — the batch build now, flush, coalesce and
/// fold in the lifecycle epic — writes through this, so the artefact has one writer rather than
/// producers that agree.
pub struct RecordBlobWriter {
    hasrow_path: PathBuf,
    directory_path: PathBuf,
    target: usize,
    blocks: BufWriter<File>,
    /// Bytes already streamed to `blocks.bin` — the next sealed block's compressed offset.
    written: u64,
    /// The current, unsealed block's uncompressed bytes.
    buf: Vec<u8>,
    /// The current block's within-block row offsets, moved into `row_offsets` at seal.
    current_offsets: Vec<u32>,
    /// The rank of the current block's first row.
    block_first_rank: u32,
    /// Sealed blocks: `(compressed_offset, compressed_len, uncompressed_len, first_rank)`.
    directory: Vec<(u64, u64, u32, u32)>,
    /// Every sealed block's row offsets, flattened; `list_offsets` carries the block boundaries.
    row_offsets: Vec<u32>,
    list_offsets: Vec<i64>,
    hasrow: Bitmap,
    rank: u32,
    last_entity: Option<u32>,
}

impl RecordBlobWriter {
    /// Create a writer over the three paths. `target` is the uncompressed block target in bytes —
    /// [`tessera_filter::RECORD_BLOCK_TARGET`] everywhere but a test that wants small blocks.
    pub fn create(
        blocks_path: &Path,
        hasrow_path: &Path,
        directory_path: &Path,
        target: usize,
    ) -> io::Result<Self> {
        if target == 0 {
            return Err(invalid("a zero block target would seal a block per row"));
        }
        let file = File::create(blocks_path)?;
        Ok(RecordBlobWriter {
            hasrow_path: hasrow_path.to_path_buf(),
            directory_path: directory_path.to_path_buf(),
            target,
            blocks: BufWriter::new(file),
            written: 0,
            buf: Vec::new(),
            current_offsets: Vec::new(),
            block_first_rank: 0,
            directory: Vec::new(),
            row_offsets: Vec::new(),
            list_offsets: vec![0],
            hasrow: Bitmap::new(),
            rank: 0,
            last_entity: None,
        })
    }

    /// Append one entity's row. Entities must ascend strictly; the fields are one entity's whole
    /// blob-resident record, encoded by the format's owner (which refuses an empty field list, a
    /// duplicate tag, and — until epic 3 — a list value).
    pub fn push_row(&mut self, entity: u32, fields: &[RecordField]) -> io::Result<()> {
        if self.last_entity.is_some_and(|last| last >= entity) {
            return Err(invalid(format!(
                "entity {entity} arrived at or below its predecessor {}; rows are in entity \
                 order (I9) and a writer that sorted would paper over a broken producer",
                self.last_entity.expect("checked is_some"),
            )));
        }
        let row_start = self.buf.len();
        encode_row(entity, fields, &mut self.buf)?;
        let row_len = self.buf.len() - row_start;

        // The row was appended to the open block optimistically; if it belongs in the next block
        // — the open block is non-empty and now past the target — move it. A row past the target
        // on its own stays: an oversized block of its own is the rule (records §3).
        if row_start > 0 && self.buf.len() > self.target {
            let row = self.buf.split_off(row_start);
            self.seal_block()?;
            self.buf = row;
        }
        let offset = self.buf.len() - row_len;
        let offset = u32::try_from(offset).map_err(|_| {
            invalid(format!(
                "entity {entity}'s row starts past u32::MAX bytes into its block; the \
                 within-block offsets are u32 (records §3)"
            ))
        })?;
        self.current_offsets.push(offset);
        self.hasrow.add(entity);
        self.last_entity = Some(entity);
        self.rank = self.rank.checked_add(1).ok_or_else(|| {
            invalid("more rows than the u32 rank space holds, which the entity ceiling forbids")
        })?;
        Ok(())
    }

    /// Compress and stream the open block, and record its directory row.
    fn seal_block(&mut self) -> io::Result<()> {
        if self.buf.is_empty() {
            return Ok(());
        }
        let uncompressed = u32::try_from(self.buf.len())
            .map_err(|_| invalid("a block exceeds u32::MAX uncompressed bytes"))?;
        let compressed = zstd::bulk::compress(&self.buf, ZSTD_LEVEL)?;
        self.blocks.write_all(&compressed)?;
        self.directory.push((
            self.written,
            compressed.len() as u64,
            uncompressed,
            self.block_first_rank,
        ));
        self.written += compressed.len() as u64;
        self.row_offsets.append(&mut self.current_offsets);
        self.list_offsets.push(self.row_offsets.len() as i64);
        self.block_first_rank = self.rank;
        self.buf.clear();
        Ok(())
    }

    /// Seal the open block and write the addressing files. `blocks.bin` is durable before the
    /// directory that addresses into it exists.
    pub fn finish(mut self) -> io::Result<()> {
        self.seal_block()?;
        let file = self
            .blocks
            .into_inner()
            .map_err(io::IntoInnerError::into_error)?;
        file.sync_all()?;

        let n = self.directory.len();
        let schema = Arc::new(Schema::new(vec![
            Field::new("compressed_offset", DataType::UInt64, false),
            Field::new("compressed_len", DataType::UInt64, false),
            Field::new("uncompressed_len", DataType::UInt32, false),
            Field::new("first_rank", DataType::UInt32, false),
            Field::new(
                "row_offsets",
                DataType::LargeList(Arc::new(Field::new("item", DataType::UInt32, false))),
                false,
            ),
        ]));
        let lists = LargeListArray::new(
            Arc::new(Field::new("item", DataType::UInt32, false)),
            OffsetBuffer::new(ScalarBuffer::from(self.list_offsets)),
            Arc::new(UInt32Array::from(self.row_offsets)),
            None,
        );
        let columns: Vec<ArrayRef> = vec![
            Arc::new(UInt64Array::from_iter_values(
                self.directory.iter().map(|d| d.0),
            )),
            Arc::new(UInt64Array::from_iter_values(
                self.directory.iter().map(|d| d.1),
            )),
            Arc::new(UInt32Array::from_iter_values(
                self.directory.iter().map(|d| d.2),
            )),
            Arc::new(UInt32Array::from_iter_values(
                self.directory.iter().map(|d| d.3),
            )),
            Arc::new(lists),
        ];
        let batch = RecordBatch::try_new(schema.clone(), columns)
            .map_err(|e| invalid(format!("assembling the block directory ({n} blocks): {e}")))?;
        let file = File::create(&self.directory_path)?;
        let mut writer = arrow::ipc::writer::FileWriter::try_new(file, &schema)
            .map_err(|e| io::Error::other(e.to_string()))?;
        writer
            .write(&batch)
            .map_err(|e| io::Error::other(e.to_string()))?;
        writer
            .finish()
            .map_err(|e| io::Error::other(e.to_string()))?;

        // Run-optimised so the file's bytes are a function of the entity set alone — the same
        // canonicalisation the presence bitmap gets in `values_writer::presence_bytes`, and for
        // the same byte-identity reason: the fold's blob and a fresh build's must not differ by
        // how a bitmap happened to be built.
        self.hasrow.run_optimize();
        std::fs::write(&self.hasrow_path, self.hasrow.serialize::<Portable>())?;
        Ok(())
    }
}

fn invalid(message: impl Into<String>) -> io::Error {
    io::Error::new(io::ErrorKind::InvalidData, message.into())
}
