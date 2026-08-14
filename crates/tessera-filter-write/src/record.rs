//! Writing the record blob: rows in, three files out (`records-and-search.md` §3, §7).
//!
//! The byte format — rows, blocks, the directory — is owned by `tessera_filter::record`, and this
//! writer is its producer half: it lives in this crate for the module doc's codegen reason (the
//! scan's crate holds no write machinery it can avoid holding), and it calls
//! [`tessera_filter::encode_row`] rather than encoding anything itself, so the layout stays one
//! module's fact.
//!
//! # The blob's lifecycle merges, and the two removal rules
//!
//! [`coalesce_record_extents`] and [`fold_record_blob`] are the value-column pair's counterparts
//! (`coalesce_attr_extents`, `fold_value_column`): the same linear merge under the same
//! non-interleaving guard, streaming rows through [`RecordBlobWriter`] — which is what repacks
//! small blocks toward the 256 KiB target as a side effect of re-blocking, rather than as a pass
//! of its own. Each input layer streams through [`tessera_filter::RecordBlob::for_each_row`],
//! whose walk *is* the addressing self-check, so a defective input refuses the pass instead of
//! being laundered into a clean-looking output.
//!
//! Write-path §5.4's two removal rules are the sharp edge, and the signatures are shaped so
//! conflating them is unspellable here exactly as they are for the value column: the coalesce has
//! **no tombstone parameter** — a suppressed *or deleted-but-unfolded* entity's row rides through
//! byte-preserved, because a suppression touches no blob byte ever (Rule S) and a deletion's
//! removal belongs to the fold alone (Rule F). The fold takes `D₀` and blanks by *remove, emit no
//! bytes*: a blanked entity leaves the has-row bitmap and contributes nothing to any block, so its
//! prose is physically absent from the folded artefact — the retention asymmetry (records §7) that
//! is the whole reason the blob lives under `attrs/` and folds with everything else rather than in
//! a store the fold does not touch.
//!
//! The fold's output is byte-identical to a fresh build's over the surviving entities: both
//! producers stream rows in entity order through this one writer at the same target, the row
//! encoding is deterministic, and the has-row bitmap is canonicalised at finish. That equality is
//! asserted by test rather than assumed.
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

use tessera_filter::{encode_row, RecordBlob, RecordError, RecordField};

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

/// A window of record-blob extents merged into one, for the entity-space coalesce (records §7:
/// the record axis beside the attribute axis, merging by concatenation in ascending entity
/// order).
///
/// **A coalesce retires nothing**, so there is no tombstone parameter to pass and no way to spell
/// one: a suppressed or deleted-but-unfolded entity's row rides through untouched, because a
/// suppression never touches the blob (Rule S) and removal is the fold's (Rule F, write-path
/// §5.4). What is written is the same `(entity, row)` relation the inputs carried between them,
/// re-blocked against `target` — which is where small flush blocks repack toward the 256 KiB
/// point, as a streaming rewrite holding one uncompressed block at a time.
pub fn coalesce_record_extents(
    inputs: &[&RecordBlob],
    blocks_path: &Path,
    hasrow_path: &Path,
    directory_path: &Path,
    target: usize,
) -> io::Result<()> {
    if inputs.len() < 2 {
        return Err(invalid(format!(
            "the record-blob coalesce was given {} extents; it collapses a window of extents \
             into one and there is nothing to collapse below two",
            inputs.len()
        )));
    }
    write_merged_rows(
        inputs,
        &Bitmap::new(),
        blocks_path,
        hasrow_path,
        directory_path,
        target,
        "the record-blob coalesce",
    )
}

/// The blob's layers merged into one base with `tombstones`' rows blanked — the fold's record
/// pass, and **the only route by which a deletion removes blob bytes** (Rule F, write-path §5.4).
///
/// Blanking is *remove, emit no bytes*: a blanked entity leaves the has-row bitmap and
/// contributes nothing to any block, so its row is physically absent from the folded artefact
/// rather than overwritten — the retention asymmetry records §7 states. `layers` is the base blob
/// followed by every extent the fold consumes, in any order; the merge sorts them by their own
/// entity ranges and refuses an interleaving, exactly as the attribute pass does.
pub fn fold_record_blob(
    layers: &[&RecordBlob],
    tombstones: &Bitmap,
    blocks_path: &Path,
    hasrow_path: &Path,
    directory_path: &Path,
    target: usize,
) -> io::Result<()> {
    if layers.is_empty() {
        return Err(invalid(
            "the fold's record pass was given no layers; a schema with a blob-resident column \
             always has at least the base blob",
        ));
    }
    write_merged_rows(
        layers,
        tombstones,
        blocks_path,
        hasrow_path,
        directory_path,
        target,
        "the fold's record pass",
    )
}

/// Stream the layers' rows into one blob in entity order, skipping `tombstones`.
///
/// The order and the two refusals are [`crate::ordered_disjoint`]'s — the same guard the value
/// columns merge under, over the layers' has-row bitmaps. Each layer then streams through
/// [`RecordBlob::for_each_row`], whose walk revalidates the input's addressing as the rows are
/// read; [`RecordBlobWriter::push_row`]'s strictly-ascending check stands behind the guard as the
/// second line.
fn write_merged_rows(
    layers: &[&RecordBlob],
    tombstones: &Bitmap,
    blocks_path: &Path,
    hasrow_path: &Path,
    directory_path: &Path,
    target: usize,
    pass: &str,
) -> io::Result<()> {
    let present: Vec<(usize, Bitmap)> = layers
        .iter()
        .enumerate()
        .map(|(i, layer)| (i, layer.hasrow().clone()))
        .collect();
    let (order, _) = crate::ordered_disjoint(present, pass)?;
    let mut writer = RecordBlobWriter::create(blocks_path, hasrow_path, directory_path, target)?;
    for (layer, _) in &order {
        layers[*layer]
            .for_each_row(&mut |entity, fields| {
                if tombstones.contains(entity) {
                    // Rule F's remove-emit-no-bytes: the row leaves the artefact by never being
                    // written, not by being overwritten.
                    return Ok(());
                }
                writer.push_row(entity, &fields).map_err(RecordError::from)
            })
            .map_err(io::Error::from)?;
    }
    writer.finish()
}

fn invalid(message: impl Into<String>) -> io::Error {
    io::Error::new(io::ErrorKind::InvalidData, message.into())
}

#[cfg(test)]
mod tests {
    use super::*;
    use tessera_filter::{Access, RecordValue, RECORD_BLOCK_TARGET};

    /// One layer's three paths under `dir`, tagged so a test can hold several.
    fn paths_of(dir: &Path, tag: &str) -> (PathBuf, PathBuf, PathBuf) {
        (
            dir.join(format!("{tag}.blocks.bin")),
            dir.join(format!("{tag}.hasrow.roaring")),
            dir.join(format!("{tag}.directory.arrow")),
        )
    }

    /// Write one layer of `(entity, note)` rows at `target` and open it back. Each row carries a
    /// utf8 field and an i64, so a row is self-describing in more than one kind.
    fn layer_at(dir: &Path, tag: &str, target: usize, rows: &[(u32, &str)]) -> RecordBlob {
        let (blocks, hasrow, directory) = paths_of(dir, tag);
        let mut writer =
            RecordBlobWriter::create(&blocks, &hasrow, &directory, target).expect("create");
        for (entity, note) in rows {
            writer
                .push_row(
                    *entity,
                    &[
                        RecordField {
                            tag: 0,
                            value: RecordValue::Utf8((*note).to_string()),
                        },
                        RecordField {
                            tag: 1,
                            value: RecordValue::I64(i64::from(*entity) * 7),
                        },
                    ],
                )
                .expect("push");
        }
        writer.finish().expect("finish");
        RecordBlob::open(&blocks, &hasrow, &directory, Access::Read).expect("open")
    }

    fn coalesce_to(
        dir: &Path,
        tag: &str,
        inputs: &[&RecordBlob],
        target: usize,
    ) -> io::Result<RecordBlob> {
        let (blocks, hasrow, directory) = paths_of(dir, tag);
        coalesce_record_extents(inputs, &blocks, &hasrow, &directory, target)?;
        Ok(RecordBlob::open(
            &blocks,
            &hasrow,
            &directory,
            Access::Read,
        )?)
    }

    /// Every `(entity, fields)` pair a blob holds, in order.
    fn rows_of(blob: &RecordBlob) -> Vec<(u32, Vec<RecordField>)> {
        let mut out = Vec::new();
        blob.for_each_row(&mut |entity, fields| {
            out.push((entity, fields));
            Ok(())
        })
        .expect("the walk");
        out
    }

    /// **A coalesced extent answers exactly as the uncoalesced layers do** — the differential the
    /// axis's content-preserving claim rests on — and a coalesced extent coalesces again, which is
    /// the recursion the manifest's window-in-place splice relies on.
    #[test]
    fn a_coalesced_extent_carries_its_inputs_rows_and_coalesces_again() {
        let dir = tempfile::tempdir().expect("tempdir");
        let extents: Vec<RecordBlob> = [100u32, 200, 300, 400]
            .iter()
            .map(|base| {
                let rows: Vec<(u32, String)> = (0..3)
                    .map(|k| (base + k * 2, format!("value-{}", base + k * 2)))
                    .collect();
                let rows: Vec<(u32, &str)> = rows.iter().map(|(e, s)| (*e, s.as_str())).collect();
                layer_at(
                    dir.path(),
                    &format!("in-{base}"),
                    RECORD_BLOCK_TARGET,
                    &rows,
                )
            })
            .collect();
        let refs: Vec<&RecordBlob> = extents.iter().collect();

        let first =
            coalesce_to(dir.path(), "first", &refs[..2], RECORD_BLOCK_TARGET).expect("coalesce");
        let second =
            coalesce_to(dir.path(), "second", &refs[2..], RECORD_BLOCK_TARGET).expect("coalesce");
        let again = coalesce_to(dir.path(), "again", &[&first, &second], RECORD_BLOCK_TARGET)
            .expect("the recursion");
        again.self_check().expect("the recursion's addressing");

        let mut expected = Vec::new();
        for extent in &extents {
            expected.extend(rows_of(extent));
        }
        assert_eq!(
            rows_of(&again),
            expected,
            "the union of the layers, in entity order"
        );
        for (entity, fields) in &expected {
            assert_eq!(
                again.fields_of(*entity).expect("read").as_deref(),
                Some(fields.as_slice()),
                "entity {entity} answers differently through the coalesced extent"
            );
        }
    }

    /// **The repack is real**: inputs cut into many tiny blocks re-block toward the target — one
    /// block out, here — with every row surviving the block-boundary crossings intact.
    #[test]
    fn small_blocks_repack_toward_the_target_and_every_row_survives() {
        let dir = tempfile::tempdir().expect("tempdir");
        // A ~40-byte row against a 64-byte target: every input block holds one row.
        let rows_a: Vec<(u32, String)> = (0..20u32)
            .map(|e| (e, format!("padding-padding-{e:04}")))
            .collect();
        let rows_b: Vec<(u32, String)> = (100..120u32)
            .map(|e| (e, format!("padding-padding-{e:04}")))
            .collect();
        let as_refs = |rows: &[(u32, String)]| -> Vec<(u32, String)> { rows.to_vec() };
        let a_rows = as_refs(&rows_a);
        let a_refs: Vec<(u32, &str)> = a_rows.iter().map(|(e, s)| (*e, s.as_str())).collect();
        let b_rows = as_refs(&rows_b);
        let b_refs: Vec<(u32, &str)> = b_rows.iter().map(|(e, s)| (*e, s.as_str())).collect();
        let a = layer_at(dir.path(), "a", 64, &a_refs);
        let b = layer_at(dir.path(), "b", 64, &b_refs);
        assert!(
            a.block_count() >= 10,
            "the fixture must be fragmented: {}",
            a.block_count()
        );

        let out = coalesce_to(dir.path(), "out", &[&a, &b], RECORD_BLOCK_TARGET).expect("coalesce");
        out.self_check().expect("addressing");
        assert_eq!(
            out.block_count(),
            1,
            "forty tiny rows repack into one target-sized block"
        );
        assert_eq!(out.rows(), 40);
        for (entity, fields) in rows_of(&a).into_iter().chain(rows_of(&b)) {
            assert_eq!(out.fields_of(entity).expect("read"), Some(fields));
        }
    }

    /// **An oversize row survives the repack in an oversized block of its own** — the target is a
    /// target, not a cap (records §3) — and its neighbours still pack normally around it.
    #[test]
    fn an_oversize_row_survives_the_repack() {
        let dir = tempfile::tempdir().expect("tempdir");
        let big = "x".repeat(4096);
        let rows: Vec<(u32, &str)> = vec![(1, "small"), (2, &big), (3, "also-small")];
        let a = layer_at(dir.path(), "a", 512, &rows);
        let b = layer_at(dir.path(), "b", 512, &[(10, "after")]);

        let out = coalesce_to(dir.path(), "out", &[&a, &b], 512).expect("coalesce");
        out.self_check().expect("addressing");
        let fields = out.fields_of(2).expect("read").expect("the oversize row");
        assert_eq!(fields[0].value, RecordValue::Utf8(big));
        for entity in [1u32, 3, 10] {
            assert!(
                out.fields_of(entity).expect("read").is_some(),
                "entity {entity}"
            );
        }
    }

    /// **A coalesce retires nothing, and there is no way to spell one that does**: a
    /// deleted-but-unfolded entity is in the overlay, not in any artefact, and its row rides
    /// through byte-preserved. A pass that blanked here would be a third retirement route — how
    /// Rule S and Rule F get conflated (write-path §5.4).
    #[test]
    fn a_coalesce_carries_every_row_through_including_a_deleted_but_unfolded_ones() {
        let dir = tempfile::tempdir().expect("tempdir");
        let a = layer_at(
            dir.path(),
            "a",
            RECORD_BLOCK_TARGET,
            &[(100, "kept"), (101, "kept-too")],
        );
        // Entity 200 is deleted-but-unfolded — a fact the overlay holds and no artefact may act on.
        let b = layer_at(
            dir.path(),
            "b",
            RECORD_BLOCK_TARGET,
            &[(200, "deleted-payload"), (201, "kept")],
        );
        let out =
            coalesce_to(dir.path(), "kept", &[&a, &b], RECORD_BLOCK_TARGET).expect("coalesce");
        assert_eq!(
            out.fields_of(200)
                .expect("read")
                .expect("the row rides through")[0]
                .value,
            RecordValue::Utf8("deleted-payload".to_string()),
        );
        assert_eq!(out.rows(), 4);
    }

    /// **The fold's output is byte-identical to a fresh build's over the survivors** — all three
    /// files — because both stream rows in entity order through the one writer at one target and
    /// the writer canonicalises its bitmap. The equality the module doc claims, held by test.
    #[test]
    fn a_folded_blob_is_the_bytes_a_single_build_would_have_written() {
        let dir = tempfile::tempdir().expect("tempdir");
        let base = layer_at(
            dir.path(),
            "base",
            RECORD_BLOCK_TARGET,
            &[(0, "zero"), (1, "one"), (2, "two")],
        );
        let ext = layer_at(
            dir.path(),
            "ext",
            RECORD_BLOCK_TARGET,
            &[(10, "ten"), (11, "eleven")],
        );
        let mut tombstones = Bitmap::new();
        tombstones.add(1);
        tombstones.add(10);

        let (blocks, hasrow, directory) = paths_of(dir.path(), "folded");
        fold_record_blob(
            &[&base, &ext],
            &tombstones,
            &blocks,
            &hasrow,
            &directory,
            RECORD_BLOCK_TARGET,
        )
        .expect("the fold");
        let _ = layer_at(
            dir.path(),
            "fresh",
            RECORD_BLOCK_TARGET,
            &[(0, "zero"), (2, "two"), (11, "eleven")],
        );
        let (f_blocks, f_hasrow, f_directory) = paths_of(dir.path(), "fresh");
        for (folded, fresh) in [
            (&blocks, &f_blocks),
            (&hasrow, &f_hasrow),
            (&directory, &f_directory),
        ] {
            assert_eq!(
                std::fs::read(folded).expect("folded"),
                std::fs::read(fresh).expect("fresh"),
                "{} differs from a fresh build's",
                folded.display()
            );
        }
    }

    /// **Blanking is remove-emit-no-bytes, asserted on the artefact bytes**: the blanked row's
    /// value is absent from the folded blocks once decompressed, its entity is out of has-row, and
    /// the survivors still answer. Rule F's whole retention claim, at the file.
    #[test]
    fn a_blanked_rows_bytes_are_not_in_the_folded_blob() {
        let dir = tempfile::tempdir().expect("tempdir");
        let base = layer_at(
            dir.path(),
            "base",
            RECORD_BLOCK_TARGET,
            &[(0, "alpha"), (1, "the-deleted-prose"), (2, "charlie")],
        );
        let mut tombstones = Bitmap::new();
        tombstones.add(1);
        let (blocks, hasrow, directory) = paths_of(dir.path(), "folded");
        fold_record_blob(
            &[&base],
            &tombstones,
            &blocks,
            &hasrow,
            &directory,
            RECORD_BLOCK_TARGET,
        )
        .expect("the fold");

        // `blocks.bin` is concatenated zstd frames; decoding them all gives every byte the folded
        // artefact can ever serve.
        let raw = std::fs::read(&blocks).expect("blocks");
        let decompressed = zstd::stream::decode_all(raw.as_slice()).expect("frames decode");
        let holds = |needle: &[u8]| decompressed.windows(needle.len()).any(|w| w == needle);
        assert!(
            !holds(b"the-deleted-prose"),
            "the blanked row's bytes are still in the folded artefact"
        );
        for kept in [&b"alpha"[..], &b"charlie"[..]] {
            assert!(holds(kept), "a survivor's bytes are gone");
        }
        let folded = RecordBlob::open(&blocks, &hasrow, &directory, Access::Read).expect("open");
        assert!(!folded.has_row(1), "the blanked entity is out of has-row");
        assert!(folded.fields_of(1).expect("read").is_none());
        assert!(folded.fields_of(0).expect("read").is_some());
    }

    /// The two refusals are this merge's own, exactly as they are the value columns': an overlap
    /// or an interleaving would pair rows with the wrong ranks with no later symptom, and a single
    /// input is not a window.
    #[test]
    fn overlapping_or_interleaved_layers_are_refused() {
        let dir = tempfile::tempdir().expect("tempdir");
        let a = layer_at(
            dir.path(),
            "a",
            RECORD_BLOCK_TARGET,
            &[(1, "one"), (3, "three")],
        );
        let clash = layer_at(dir.path(), "clash", RECORD_BLOCK_TARGET, &[(3, "again")]);
        let err = coalesce_to(dir.path(), "c1", &[&a, &clash], RECORD_BLOCK_TARGET)
            .expect_err("an overlap is refused");
        assert!(err.to_string().contains("twice"), "{err}");

        let interleaved = layer_at(
            dir.path(),
            "b",
            RECORD_BLOCK_TARGET,
            &[(0, "zero"), (2, "two")],
        );
        let err = coalesce_to(dir.path(), "c2", &[&a, &interleaved], RECORD_BLOCK_TARGET)
            .expect_err("interleaving is refused");
        assert!(err.to_string().contains("interleaved"), "{err}");

        assert!(coalesce_to(dir.path(), "c3", &[&a], RECORD_BLOCK_TARGET).is_err());
    }
}
