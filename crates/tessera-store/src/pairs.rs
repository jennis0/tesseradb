//! `terms/pairs.parquet` — the `(entity_id, term_id)` relation the I1 mask differential runs
//! against (contracts §2.4).
//!
//! **A bundle artefact, so its writer lives with the others.** `SegmentWriter`, `PermutationWriter`,
//! `RunWriter`, `LocatorWriter` and `write_segments_manifest` all sit in this crate because they
//! write files contracts §2 defines; this one wrote from `tessera-build` alone for as long as a
//! build was the only thing that produced it. Compaction's pass 2 is the second producer, and it
//! cannot reach `tessera-build` — that crate already depends on `tessera-authz`, so the edge only
//! runs one way, and the fold's driver in `tessera-engine` has no edge to it either. Moving the
//! writer down to the crate both already depend on is the placement the other six artefacts already
//! have, rather than a new exception for this one.
//!
//! **Optional for a serving deployment, required for a conformance run** (contracts §2.4). That
//! makes it optional to *read*, never optional to *write*: a fold that skipped it would leave a
//! compacted bundle the conformance suite cannot run against, and the suite is the deliverable.

use std::fs::File;
use std::path::{Path, PathBuf};

use arrow::array::{ArrayRef, UInt32Array, UInt64Array};
use arrow::datatypes::{DataType, Field, Schema};
use arrow::record_batch::RecordBatch;
use parquet::arrow::ArrowWriter;
use parquet::basic::{Compression, Encoding};
use parquet::file::properties::{EnabledStatistics, WriterProperties, WriterVersion};

use crate::error::{Result, StoreError};

/// Write `pairs.parquet` (R4): `(entity_id: uint64, term_id: uint32)` sorted by
/// `(term_id, entity_id)`, DELTA_BINARY_PACKED on both columns.
///
/// Rows are pushed in the required order rather than sorted here — both builds emit terms in
/// ordinal order and each term's entities ascending, so the sort *is* the iteration order. The
/// file is off both request paths (build-cadence and oracle reads only), so the encoding is
/// chosen for the oracle's benefit, not for query latency.
///
/// **Two producers, which is why this lives here and not in `tessera-build`.** A build emits this
/// file once; compaction's pass 2 re-emits it at every fold, because a carried-forward
/// `pairs.parquet` would disagree with the new base postings about every folded deletion — the one
/// disagreement the I1 differential exists to catch (compaction §3). The build's copy was the
/// original home only because a build was the only producer; a bundle artefact with two producers
/// belongs beside the writers of the other six.
pub struct PairsParquetWriter {
    path: PathBuf,
    schema: std::sync::Arc<Schema>,
    writer: ArrowWriter<File>,
    entities: Vec<u64>,
    terms: Vec<u32>,
}

impl PairsParquetWriter {
    /// Rows per record batch. Bounds the writer's own memory no matter how many pairs arrive.
    const BATCH: usize = 1 << 16;

    pub fn create(path: &Path) -> Result<Self> {
        let schema = std::sync::Arc::new(Schema::new(vec![
            Field::new("entity_id", DataType::UInt64, false),
            Field::new("term_id", DataType::UInt32, false),
        ]));
        let props = WriterProperties::builder()
            .set_writer_version(WriterVersion::PARQUET_2_0)
            .set_encoding(Encoding::DELTA_BINARY_PACKED)
            .set_dictionary_enabled(false)
            .set_statistics_enabled(EnabledStatistics::Chunk)
            .set_compression(Compression::SNAPPY)
            .build();
        let file = File::create(path).map_err(|e| StoreError::Io { path: path.to_path_buf(), source: e })?;
        let writer = ArrowWriter::try_new(file, schema.clone(), Some(props))
            .map_err(|e| StoreError::Parquet { path: path.to_path_buf(), detail: e.to_string() })?;
        Ok(PairsParquetWriter {
            path: path.to_path_buf(),
            schema,
            writer,
            entities: Vec::with_capacity(Self::BATCH),
            terms: Vec::with_capacity(Self::BATCH),
        })
    }
    /// Push one term's whole (ascending) entity list. Batch boundaries fall at exactly the
    /// rows they would under per-row [`push`] — fill to `BATCH`, flush, continue — so the
    /// file bytes are identical; only the 1.7 × 10⁹ call-per-row overhead is gone.
    pub fn push_run(&mut self, term_id: u32, entities: &[u32]) -> Result<()> {
        let mut rest = entities;
        while !rest.is_empty() {
            let take = (Self::BATCH - self.entities.len()).min(rest.len());
            let (now, later) = rest.split_at(take);
            self.entities.extend(now.iter().map(|&e| e as u64));
            self.terms.extend(std::iter::repeat_n(term_id, now.len()));
            if self.entities.len() == Self::BATCH {
                self.flush()?;
            }
            rest = later;
        }
        Ok(())
    }

    /// Push one term's final entity set from an iterator, in the same batches [`Self::push_run`]
    /// would produce from an equivalent slice — fill to `BATCH`, flush, continue — so the file
    /// bytes are identical; only the caller's obligation to hold the set as a `Vec<u32>` first is
    /// gone.
    ///
    /// **This is what lets the term sweep drive this writer without a `Vec<u32>`.** The sweep's
    /// accumulator is a `croaring::Bitmap`, and the widest term is a measured 125.12 MB as
    /// portable Roaring against 2 GB as `u32`s at 10⁹ (compaction §3) — collecting
    /// `Bitmap::iter()` into a `Vec<u32>` before calling [`Self::push_run`] would reintroduce
    /// exactly the memory bound that measurement is about. `Bitmap::iter()` yields ascending,
    /// which is the order this file requires and the order [`Self::push_run`] already assumes.
    pub fn push_iter(&mut self, term_id: u32, entities: impl Iterator<Item = u32>) -> Result<()> {
        for entity_id in entities {
            self.entities.push(entity_id as u64);
            self.terms.push(term_id);
            if self.entities.len() == Self::BATCH {
                self.flush()?;
            }
        }
        Ok(())
    }

    fn flush(&mut self) -> Result<()> {
        if self.entities.is_empty() {
            return Ok(());
        }
        let batch = RecordBatch::try_new(
            self.schema.clone(),
            vec![
                std::sync::Arc::new(UInt64Array::from(std::mem::take(&mut self.entities)))
                    as ArrayRef,
                std::sync::Arc::new(UInt32Array::from(std::mem::take(&mut self.terms))) as ArrayRef,
            ],
        )
        .map_err(|e| StoreError::Parquet { path: self.path.clone(), detail: e.to_string() })?;
        self.entities.reserve(Self::BATCH);
        self.terms.reserve(Self::BATCH);
        self.writer
            .write(&batch)
            .map_err(|e| StoreError::Parquet { path: self.path.clone(), detail: e.to_string() })
    }

    pub fn finish(mut self) -> Result<()> {
        self.flush()?;
        let path = self.path.clone();
        self.writer
            .close()
            .map_err(|e| StoreError::Parquet { path: path.clone(), detail: e.to_string() })?;
        fsync_file(&path)
    }
}

/// `fsync` the file and its directory, so the rename that made it visible is durable too — the
/// same discipline `tessera-build` applies to every artefact it writes, kept here because a
/// producer that skipped it would leave a `pairs.parquet` a crash could truncate.
fn fsync_file(path: &Path) -> Result<()> {
    let file = File::open(path).map_err(|e| StoreError::Io {
        path: path.to_path_buf(),
        source: e,
    })?;
    file.sync_all().map_err(|e| StoreError::Io {
        path: path.to_path_buf(),
        source: e,
    })?;
    if let Some(parent) = path.parent() {
        let dir = File::open(parent).map_err(|e| StoreError::Io {
            path: parent.to_path_buf(),
            source: e,
        })?;
        dir.sync_all().map_err(|e| StoreError::Io {
            path: parent.to_path_buf(),
            source: e,
        })?;
    }
    Ok(())
}
