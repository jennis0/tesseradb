//! A Parquet file's row groups, and where each row sits in the file.
//!
//! Every reader in the build names a row by its position in its file, because that is what the
//! rule numbered ([`crate::ids::Numbers`]). A reader decodes one row group at a time and knows the
//! group's first row from the footer, so a row's position is known whichever worker decoded it and
//! whichever groups a `--limit` left out.

use std::fs::File;
use std::ops::ControlFlow;
use std::path::Path;
use std::sync::mpsc;

use arrow::record_batch::RecordBatch;
use parquet::arrow::arrow_reader::{
    ArrowReaderMetadata, ArrowReaderOptions, ParquetRecordBatchReader,
    ParquetRecordBatchReaderBuilder,
};
use parquet::arrow::ProjectionMask;

use crate::error::{BuildError, Result};

/// How many rows a reader decodes at a time.
pub(crate) const BATCH_ROWS: usize = 65_536;

/// Decode workers at most, and decoded batches in flight between them and the caller.
const WORKERS_MAX: usize = 6;
const CHANNEL_BATCHES: usize = 16;

/// A file's footer, read once, with each row group's first row.
#[derive(Clone)]
pub(crate) struct FileGroups {
    metadata: ArrowReaderMetadata,
    starts: Vec<u64>,
    rows: u64,
}

impl FileGroups {
    pub(crate) fn open(path: &Path) -> Result<FileGroups> {
        FileGroups::open_with(path, ArrowReaderOptions::new())
    }

    /// The footer read with `options`, such as a schema asking for a column as a dictionary.
    pub(crate) fn open_with(path: &Path, options: ArrowReaderOptions) -> Result<FileGroups> {
        let file = File::open(path).map_err(|e| BuildError::io(path, e))?;
        let metadata =
            ArrowReaderMetadata::load(&file, options).map_err(|e| BuildError::parquet(path, e))?;
        let mut starts = Vec::with_capacity(metadata.metadata().num_row_groups());
        let mut rows = 0u64;
        for group in metadata.metadata().row_groups() {
            starts.push(rows);
            rows += group.num_rows().max(0) as u64;
        }
        Ok(FileGroups {
            metadata,
            starts,
            rows,
        })
    }

    pub(crate) fn schema(&self) -> &arrow::datatypes::SchemaRef {
        self.metadata.schema()
    }

    pub(crate) fn metadata(&self) -> &parquet::file::metadata::ParquetMetaData {
        self.metadata.metadata()
    }

    /// Every row the file holds.
    pub(crate) fn rows(&self) -> u64 {
        self.rows
    }

    pub(crate) fn count(&self) -> usize {
        self.starts.len()
    }

    /// The file row of row group `group`'s first row.
    pub(crate) fn start(&self, group: usize) -> u64 {
        self.starts[group]
    }

    /// How many rows row group `group` holds.
    pub(crate) fn group_rows(&self, group: usize) -> u64 {
        self.metadata.metadata().row_group(group).num_rows().max(0) as u64
    }

    /// The projection of the file's root columns at `roots`.
    pub(crate) fn projection(&self, roots: &[usize]) -> ProjectionMask {
        ProjectionMask::roots(self.metadata.metadata().file_metadata().schema_descr(), roots.to_vec())
    }

    /// A reader over one row group, with the row group's first row.
    pub(crate) fn reader(
        &self,
        path: &Path,
        group: usize,
        projection: &ProjectionMask,
    ) -> Result<(u64, ParquetRecordBatchReader)> {
        let file = File::open(path).map_err(|e| BuildError::io(path, e))?;
        let reader = ParquetRecordBatchReaderBuilder::new_with_metadata(file, self.metadata.clone())
            .with_row_groups(vec![group])
            .with_projection(projection.clone())
            .with_batch_size(BATCH_ROWS)
            .build()
            .map_err(|e| BuildError::parquet(path, e))?;
        Ok((self.starts[group], reader))
    }

    /// Every batch of `groups`, in order, with the file row of its first row, on this thread.
    pub(crate) fn each_batch(
        &self,
        path: &Path,
        groups: &[usize],
        projection: &ProjectionMask,
        mut visit: impl FnMut(u64, RecordBatch) -> Result<ControlFlow<()>>,
    ) -> Result<()> {
        for &group in groups {
            let (mut first, reader) = self.reader(path, group, projection)?;
            for batch in reader {
                let batch = batch.map_err(|e| BuildError::arrow(path, e))?;
                let len = batch.num_rows() as u64;
                if visit(first, batch)?.is_break() {
                    return Ok(());
                }
                first += len;
            }
        }
        Ok(())
    }

    /// Every batch of `groups`, each turned into a `T` by `decode` on one of a few worker threads
    /// and handed to `visit` on this one, **in no particular order**. `visit`'s `Break` stops the
    /// scan and the workers wind down.
    pub(crate) fn decode_parallel<T: Send>(
        &self,
        path: &Path,
        groups: &[usize],
        projection: &ProjectionMask,
        decode: impl Fn(u64, RecordBatch) -> Result<T> + Sync,
        mut visit: impl FnMut(T) -> Result<ControlFlow<()>>,
    ) -> Result<()> {
        let workers = std::thread::available_parallelism()
            .map(|n| n.get())
            .unwrap_or(4)
            .min(WORKERS_MAX)
            .min(groups.len())
            .max(1);
        let shards: Vec<Vec<usize>> = groups
            .chunks(groups.len().div_ceil(workers).max(1))
            .map(<[usize]>::to_vec)
            .collect();
        let (tx, rx) = mpsc::sync_channel::<Result<T>>(CHANNEL_BATCHES);
        std::thread::scope(|scope| {
            for shard in shards {
                let tx = tx.clone();
                let decode = &decode;
                scope.spawn(move || {
                    let run = || -> Result<()> {
                        for group in shard {
                            let (mut first, reader) = self.reader(path, group, projection)?;
                            for batch in reader {
                                let batch = batch.map_err(|e| BuildError::arrow(path, e))?;
                                let len = batch.num_rows() as u64;
                                if tx.send(decode(first, batch)).is_err() {
                                    // The caller stopped; stop quietly.
                                    return Ok(());
                                }
                                first += len;
                            }
                        }
                        Ok(())
                    };
                    if let Err(e) = run() {
                        let _ = tx.send(Err(e));
                    }
                });
            }
            drop(tx);
            let mut consume = || -> Result<()> {
                while let Ok(decoded) = rx.recv() {
                    if visit(decoded?)?.is_break() {
                        return Ok(());
                    }
                }
                Ok(())
            };
            let result = consume();
            // The receiver goes before the scope joins the workers: a worker blocked sending into
            // a full channel would otherwise never return.
            drop(rx);
            result
        })
    }
}
